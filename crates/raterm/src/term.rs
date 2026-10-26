use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Row as GridRow};
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
pub use alacritty_terminal::vte::ansi::CursorShape;
use alacritty_terminal::vte::ansi::{Color, CursorStyle, NamedColor, Processor};
use serde::Serialize;

use crate::graphics::kitty::{self, Command};
use crate::graphics::{Data, Image, Images, Pixels, Slice, sixel};
use crate::history::{self, Entry, History};
use crate::media::Media;
use crate::rich::{Block, Filter, Piece};

/// History is drained every `CHUNK` bytes, so it only needs to hold the lines
/// that can scroll out meanwhile (one per byte, more with `CSI S`).
const HISTORY: usize = 1 << 16;

/// Past 1,000 rows of history, alacritty frees the rows it drops, and
/// allocates them again as the next lines scroll out: draining at least that
/// often keeps them for reuse.
const CHUNK: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Modes {
    /// Arrow keys send `ESC O` sequences.
    pub app_cursor: bool,
    /// Mouse events the program asked for: 0 none, 1 clicks, 2 clicks and
    /// drags, 3 clicks and any motion.
    pub mouse: u8,
    /// Mouse events use the SGR encoding (1006) rather than the legacy one.
    pub sgr_mouse: bool,
    /// Without mouse reporting, the wheel sends arrow keys on the alternate
    /// screen (1007).
    pub alt_scroll: bool,
}

impl Modes {
    fn of(mode: &TermMode) -> Modes {
        Modes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            mouse: if mode.contains(TermMode::MOUSE_MOTION) {
                3
            } else if mode.contains(TermMode::MOUSE_DRAG) {
                2
            } else {
                mode.contains(TermMode::MOUSE_REPORT_CLICK).into()
            },
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            alt_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        }
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub runs: Vec<Run>,
    /// The line continues on the next one (soft wrap).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wrap: bool,
    /// The cells that show images. Kept apart from the runs, which would
    /// all be larger for them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Shown>,
}

/// Cells of a line that show an image: `len` cells from cell `at`, the first
/// one showing `slice`, the next ones the next columns of its row.
#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Shown {
    pub at: usize,
    pub len: usize,
    pub slice: Slice,
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct Run {
    pub text: String,
    #[serde(flatten)]
    pub style: Style,
    /// The cells the run takes as one unit, to draw whole from its first
    /// cell: a char with zero-width chars after it (combining marks), and the
    /// chars a joiner (ZWJ) puts after it; 0 for a run of plain chars.
    #[serde(skip_serializing_if = "is_zero")]
    pub cluster: u8,
}

/// Serialized as the CSS of the first versions (`var(--c1)`, `#ff8800`,
/// `"b u"`), which the snapshots keep.
#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct Style {
    /// `None` for the default colors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<Tint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<Tint>,
    #[serde(rename = "class", skip_serializing_if = "Attrs::is_empty")]
    pub attrs: Attrs,
    /// Target of an OSC 8 hyperlink, or of a URL written in the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// A color of a cell: the default ones (when inverse video swaps them), one
/// of the 16 of the palette, or RGB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tint {
    Fg,
    Bg,
    Palette(u8),
    Rgb(u32),
}

impl Serialize for Tint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Tint::Fg => s.serialize_str("var(--fg)"),
            Tint::Bg => s.serialize_str("var(--bg)"),
            Tint::Palette(i) => s.serialize_str(&format!("var(--c{i})")),
            Tint::Rgb(v) => s.serialize_str(&format!("#{v:06x}")),
        }
    }
}

/// Attributes of a cell, as bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs(u8);

impl Attrs {
    pub const BOLD: Attrs = Attrs(1);
    pub const ITALIC: Attrs = Attrs(2);
    pub const UNDERLINE: Attrs = Attrs(4);
    pub const STRIKE: Attrs = Attrs(8);
    pub const DIM: Attrs = Attrs(16);
    pub const HIDDEN: Attrs = Attrs(32);
    /// Each char takes two cells (CJK, some emoji).
    pub const WIDE: Attrs = Attrs(64);
    /// The cursor's cell, drawn according to the configured cursor style.
    pub const CURSOR: Attrs = Attrs(128);
    const NAMES: [(Attrs, &str); 8] = [
        (Attrs::BOLD, "b"),
        (Attrs::ITALIC, "i"),
        (Attrs::UNDERLINE, "u"),
        (Attrs::STRIKE, "s"),
        (Attrs::DIM, "d"),
        (Attrs::HIDDEN, "h"),
        (Attrs::WIDE, "wide"),
        (Attrs::CURSOR, "cursor"),
    ];

    pub fn has(self, a: Attrs) -> bool {
        self.0 & a.0 != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Attrs {
    type Output = Attrs;
    fn bitor(self, a: Attrs) -> Attrs {
        Attrs(self.0 | a.0)
    }
}

impl Serialize for Attrs {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let names: Vec<&str> = Attrs::NAMES
            .iter()
            .filter(|(a, _)| self.has(*a))
            .map(|(_, name)| *name)
            .collect();
        s.serialize_str(&names.join(" "))
    }
}

struct Listener(Sender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.send(event);
    }
}

/// What a batch of output changed, for the view; it reads the rest from the
/// terminal's state.
#[derive(Debug, Default)]
pub struct Update {
    /// The screen or the scrollback may have changed: the view has to be
    /// drawn again.
    pub changed: bool,
    /// Window title set by the program (OSC 0/2); empty to reset it.
    pub title: Option<String>,
    /// The modes, when they changed.
    pub modes: Option<Modes>,
    /// Bytes the terminal answers to the program (e.g. device status reports).
    pub reply: String,
}

/// A match of a search: an entry of the scrollback, or `next + y` for row `y`
/// of the screen, and its start and end in the entry's text (UTF-16 units of
/// the lowercased text).
pub type Found = (u64, usize, usize);

/// A placement of kitty's on the alternate screen, kept apart from its cells:
/// text written over it does not erase it, as in kitty, where full-screen
/// programs send an image, then redraw the text of their frame over it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Overlay {
    image: u32,
    /// Its first row on the screen, above it once scrolled out.
    line: i64,
    col: usize,
    cols: usize,
    rows: usize,
}

impl Overlay {
    fn covers(&self, line: i64, col: usize) -> bool {
        (self.line..self.line + self.rows as i64).contains(&line)
            && (self.col..self.col + self.cols).contains(&col)
    }
}

pub struct Terminal {
    /// Bold text in one of the 8 normal colors uses its bright variant.
    pub bold_is_bright: bool,
    term: Term<Listener>,
    parser: Processor,
    term_events: Receiver<TermEvent>,
    /// Replies of the core's own, sent along with `alacritty_terminal`'s to
    /// keep their order: programs read up to the last answer they expect.
    replies: Sender<TermEvent>,
    /// The size of a cell in pixels, as the view measured it.
    cell: (f64, f64),
    filter: Filter,
    /// Local files shown by video blocks.
    pub media: Media,
    /// The images cells show.
    pub images: Images,
    chunks: kitty::Chunks,
    overlays: Vec<Overlay>,
    /// Cells changed out of `alacritty_terminal`'s sight, which tracks the
    /// lines it changes: the whole screen is rendered again.
    full: bool,
    pub history: History,
    /// The screen as last rendered, and whether output came since: it is
    /// rendered when the view reads it, once per frame at most, rather than
    /// after every batch of a flood.
    screen: Vec<Line>,
    stale: bool,
    alt: bool,
    /// The modes as last reported to the view.
    modes: Modes,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        let (tx, rx) = channel();
        let config = Config {
            scrolling_history: HISTORY,
            ..Config::default()
        };
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(config, &size, Listener(tx.clone()));
        Terminal {
            bold_is_bright: false,
            // The view starts from the same modes.
            modes: Modes::of(term.mode()),
            term,
            parser: Processor::new(),
            term_events: rx,
            replies: tx,
            cell: (0.0, 0.0),
            filter: Filter::default(),
            media: Media::default(),
            images: Images::default(),
            chunks: kitty::Chunks::default(),
            overlays: Vec::new(),
            full: false,
            history: History::default(),
            screen: Vec::new(),
            stale: true,
            alt: false,
        }
    }

    /// The cursor of the configuration, which programs go back to with
    /// `CSI 0 q`.
    pub fn set_default_cursor(&mut self, shape: CursorShape, blinking: bool) {
        self.term.set_options(Config {
            scrolling_history: HISTORY,
            default_cursor_style: CursorStyle { shape, blinking },
            ..Config::default()
        });
    }

    /// The shape of the cursor and whether it blinks, as configured or as a
    /// program set them (DECSCUSR).
    pub fn cursor(&self) -> (CursorShape, bool) {
        let style = self.term.cursor_style();
        (style.shape, style.blinking)
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Update {
        let mut changed = false;
        for piece in self.filter.feed(bytes) {
            match piece {
                Piece::Bytes(bytes) => {
                    // As in kitty, clearing the screen drops its images.
                    let clears = |seq: &[u8]| bytes.windows(seq.len()).any(|w| w == seq);
                    if !self.overlays.is_empty()
                        && (clears(b"\x1b[2J") || clears(b"\x1b[3J") || clears(b"\x1bc"))
                    {
                        self.overlays.clear();
                        self.full = true;
                    }
                    // Erasing the screen pushes it to the scrollback, and
                    // `clear` erases the scrollback before the screen on
                    // macOS, after on Linux: dropped once both are read,
                    // keeping what follows in the same read (`clear && make`).
                    let mut rest = &bytes[..];
                    while let Some(at) = rest.windows(4).position(|w| w == b"\x1b[3J") {
                        let mut end = at + 4;
                        for seq in [&b"\x1b[H"[..], b"\x1b[2J"] {
                            if rest[end..].starts_with(seq) {
                                end += seq.len();
                            }
                        }
                        self.parse(&rest[..end]);
                        self.clear_history();
                        rest = &rest[end..];
                    }
                    self.parse(rest);
                    // Rendering the screen to know whether it changed would
                    // cost what rendering it lazily saves.
                    changed = true;
                }
                Piece::Block(block) => changed |= self.insert(block),
                Piece::CellSize => {
                    let (w, h) = (self.cell.0.round(), self.cell.1.round());
                    let reply = format!("\x1b[6;{h};{w}t");
                    let _ = self.replies.send(TermEvent::PtyWrite(reply));
                }
                Piece::Graphics(params) => {
                    let reply = self.graphics_attributes(&params);
                    let _ = self.replies.send(TermEvent::PtyWrite(reply));
                }
                Piece::Sixel(data) => changed |= self.place_sixel(&data),
                Piece::Kitty(data) => changed |= self.kitty(&data),
            }
        }
        self.update(changed)
    }

    /// Resizes to `cols × rows` cells of `cell` pixels.
    pub fn resize(&mut self, cols: u16, rows: u16, cell: (f64, f64)) -> Update {
        self.cell = cell;
        self.term
            .resize(TermSize::new(cols.max(1) as usize, rows.max(1) as usize));
        self.update(true)
    }

    /// The width and height of the text area in pixels.
    pub fn text_area(&self) -> (u16, u16) {
        let width = self.term.columns() as f64 * self.cell.0;
        let height = self.term.screen_lines() as f64 * self.cell.1;
        (width.round() as u16, height.round() as u16)
    }

    /// Encodes pasted text, honoring bracketed paste mode.
    pub fn paste(&self, text: &str) -> String {
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            // Stripping ESC prevents the text from closing the bracket itself.
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        }
    }

    /// The screen as of the last `refresh` (`Session::read` refreshes it):
    /// the alternate one while it is active.
    pub fn screen(&self) -> &[Line] {
        &self.screen
    }

    /// Renders the screen if output came since it was last rendered.
    pub fn refresh(&mut self) {
        if !self.stale {
            return;
        }
        self.stale = false;
        // Only the lines alacritty saw change: rendering a whole screen of
        // code took 0.4 ms, when typing changes a line or two.
        let damaged: Option<Vec<usize>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(lines) => Some(lines.map(|d| d.line).collect()),
        };
        self.term.reset_damage();
        let overlays = self.overlays.len();
        let images = &self.images;
        self.overlays.retain(|o| images.get(o.image).is_some());
        self.full |= self.overlays.len() != overlays;
        let mut screen = std::mem::take(&mut self.screen);
        let grid = self.term.grid();
        let cursor = self
            .term
            .mode()
            .contains(TermMode::SHOW_CURSOR)
            .then_some(grid.cursor.point);
        let render = |y: usize| {
            let x = cursor.filter(|c| c.line.0 == y as i32).map(|c| c.column.0);
            let mut line = self.render(&grid[GridLine(y as i32)], x);
            for o in self.overlays.iter().filter(|o| o.covers(y as i64, o.col)) {
                line.images.push(Shown {
                    at: o.col,
                    len: o.cols,
                    slice: Slice {
                        image: o.image,
                        row: (y as i64 - o.line) as u16,
                        col: 0,
                    },
                });
            }
            line
        };
        match damaged {
            Some(lines) if !self.full && screen.len() == grid.screen_lines() => {
                for y in lines {
                    screen[y] = render(y);
                }
            }
            _ => screen = (0..grid.screen_lines()).map(render).collect(),
        }
        self.screen = screen;
        self.full = false;
    }

    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// The match of `query` next to `from` (an entry and an offset in its
    /// text, see `Found`), going backwards or forwards over the lines
    /// and blocks of the scrollback and the lines of the screen, and wrapping
    /// around. Without `from`,
    /// starts from the end, or from the beginning forwards.
    pub fn search(
        &self,
        query: &str,
        from: Option<(u64, usize)>,
        backwards: bool,
    ) -> Option<Found> {
        let needle = query.to_lowercase();
        let (first, next) = (self.history.first(), self.history.next());
        let end = next + self.screen.len() as u64;
        let text = |n: u64| match self.history.get(n) {
            Some(Entry::Line(line)) => Some(line.text()),
            Some(Entry::Block(block)) => history::block_text(block),
            None => self.screen.get((n - next) as usize).map(Line::text),
        };
        let from = from.filter(|(n, _)| (first..end).contains(n));
        // An empty query matches nothing, rather than everywhere.
        let len = if needle.is_empty() { 0 } else { end - first };
        let start = match from {
            Some((n, _)) => n - first,
            None if backwards => len.saturating_sub(1),
            None => 0,
        };
        let at = from.map(|(_, at)| at);
        // The start entry comes again last, for its matches on the other side.
        let visits = if at.is_some() { len + 1 } else { len };
        for k in 0..visits {
            let i = if backwards {
                (start + len - k % len) % len
            } else {
                (start + k) % len
            };
            let Some(text) = text(first + i) else {
                continue;
            };
            let ok = |&(s, _): &(usize, usize)| match at {
                Some(at) if k == 0 => (s < at) == backwards && s != at,
                Some(at) if k == len => {
                    if backwards {
                        s >= at
                    } else {
                        s <= at
                    }
                }
                _ => true,
            };
            let found = history::matches(&text, &needle);
            let found = if backwards {
                found.into_iter().rev().find(ok)
            } else {
                found.into_iter().find(ok)
            };
            if let Some((start, end)) = found {
                return Some((first + i, start, end));
            }
        }
        None
    }

    /// Screen content as plain text, for tests.
    pub fn text(&self) -> String {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|y| self.render(&grid[GridLine(y as i32)], None).text() + "\n")
            .collect()
    }

    /// Inserts a block at the cursor: everything above it moves to the
    /// scrollback, the block follows, and the cursor starts the next line.
    /// False when nothing changed.
    fn insert(&mut self, block: Block) -> bool {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let old = block.id.as_ref().and_then(|id| self.history.block(id));
        let block = match block.render(&self.media) {
            Ok(block) => block,
            Err(e) => {
                eprintln!("raterm: invalid rich block: {e}");
                return false;
            }
        };
        if let Some(entry) = old {
            self.history.replace(entry, Entry::Block(block));
            return true;
        }
        if self.term.grid().cursor.point.column.0 > 0 {
            self.parser.advance(&mut self.term, b"\r\n");
        }
        let y = self.term.grid().cursor.point.line.0;
        if y > 0 {
            let scroll = format!("\x1b[{y}S\x1b[H");
            self.parser.advance(&mut self.term, scroll.as_bytes());
        }
        self.drain_history();
        self.history.push(Entry::Block(block));
        true
    }

    /// The answer to XTSMGRAPHICS: the registers of Sixel images, and their
    /// largest size, the text area's; setting them keeps them as they are.
    fn graphics_attributes(&self, params: &[usize]) -> String {
        let (item, action) = (params[0], params.get(1).copied().unwrap_or(0));
        let (w, h) = self.text_area();
        match (item, action) {
            (1, 1..=4) => format!("\x1b[?1;0;{}S", sixel::REGISTERS),
            (2, 1..=4) => format!("\x1b[?2;0;{w};{h}S"),
            (1 | 2, _) => format!("\x1b[?{item};2;0S"),
            _ => format!("\x1b[?{item};1;0S"),
        }
    }

    /// Places a Sixel image at the cursor, which ends on the row below it,
    /// in the same column. False when nothing changed.
    fn place_sixel(&mut self, data: &[u8]) -> bool {
        let (cw, ch) = self.cell;
        let (w, h) = self.text_area();
        if cw < 1.0 || ch < 1.0 {
            return false;
        }
        let Some((width, height, pixels)) = sixel::decode(data, (w as usize, h as usize)) else {
            return false;
        };
        let cols = (width as f64 / cw).ceil() as usize;
        let rows = (height as f64 / ch).ceil() as usize;
        let data = Arc::new(Data {
            width,
            height,
            pixels: Pixels::Argb(pixels),
        });
        let bytes = data.size();
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let start = self.place(Image::whole(data, self.cell, alt), bytes, cols, rows, false);
        let below = format!("\n\x1b[{}G", start + 1);
        self.parser.advance(&mut self.term, below.as_bytes());
        self.drain_history();
        true
    }

    /// Marks `cols × rows` cells from the cursor with the slices of `image`,
    /// or keeps it `apart` from them (see `Overlay`), counting `bytes` in the
    /// quota, a row at a time and scrolling at the bottom: the cursor ends on
    /// its last row. The column it started at.
    fn place(
        &mut self,
        image: Image,
        bytes: usize,
        cols: usize,
        rows: usize,
        apart: bool,
    ) -> usize {
        if image.alt {
            self.drop_unseen_alt_images();
        }
        let image = self.images.place(image, bytes);
        let start = self.term.grid().cursor.point.column.0;
        let cols = cols.min(self.term.columns() - start);
        let first = self.term.grid().cursor.point.line.0 as i64;
        for row in 0..rows {
            if row > 0 {
                let above = self.term.grid().cursor.point.line;
                self.parser.advance(&mut self.term, b"\n");
                // At the bottom, the screen scrolled: its images with it.
                if self.term.grid().cursor.point.line == above {
                    self.overlays.iter_mut().for_each(|o| o.line -= 1);
                }
            }
            if apart {
                continue;
            }
            let line = self.term.grid().cursor.point.line;
            for col in 0..cols {
                let slice = Slice {
                    image,
                    row: row as u16,
                    col: col as u16,
                };
                self.term.grid_mut()[line][Column(start + col)].set_hyperlink(Some(slice.mark()));
            }
        }
        if apart {
            let last = self.term.grid().cursor.point.line.0 as i64;
            self.overlays.push(Overlay {
                image,
                line: (last + 1 - rows as i64).min(first),
                col: start,
                cols,
                rows,
            });
        }
        self.full = true;
        start
    }

    /// Runs a command of kitty's graphics protocol, and answers it. False
    /// when nothing changed.
    fn kitty(&mut self, data: &[u8]) -> bool {
        let Some((mut cmd, payload)) = self.chunks.feed(data) else {
            return false;
        };
        let (changed, error) = match cmd.action {
            b't' | b'T' | b'q' => match kitty::decode(&cmd, &payload) {
                Err(error) => (false, Some(error)),
                Ok(_) if cmd.action == b'q' => (false, None),
                Ok(data) => {
                    if cmd.number != 0 {
                        let picked = (cmd.id != 0).then_some(cmd.id);
                        cmd.id = self.images.number(cmd.number, picked);
                    }
                    let data = Arc::new(data);
                    if cmd.id != 0 {
                        self.images.store(cmd.id, data.clone());
                    }
                    (cmd.action == b'T' && self.put(&cmd, data), None)
                }
            },
            b'p' => {
                if cmd.id == 0 {
                    cmd.id = self.images.numbered(cmd.number).unwrap_or(0);
                }
                match self.images.stored(cmd.id).cloned() {
                    Some(data) => (self.put(&cmd, data), None),
                    None => (false, Some("ENOENT:no image with this id or number")),
                }
            }
            b'd' => return self.delete(&cmd),
            _ => return false,
        };
        if let Some(reply) = cmd.reply(error) {
            let _ = self.replies.send(TermEvent::PtyWrite(reply));
        }
        changed
    }

    /// Puts kitty's image at the cursor, which ends on its last row, in the
    /// column after it. False when nothing changed.
    fn put(&mut self, cmd: &Command, data: Arc<Data>) -> bool {
        let (cw, ch) = self.cell;
        if cw < 1.0 || ch < 1.0 {
            return false;
        }
        let (x, y) = (cmd.source.0.min(data.width), cmd.source.1.min(data.height));
        let part = |asked: usize, left: usize| if asked == 0 { left } else { asked.min(left) };
        let (w, h) = (
            part(cmd.source.2, data.width - x),
            part(cmd.source.3, data.height - y),
        );
        if w == 0 || h == 0 {
            return false;
        }
        if cmd.placeholders {
            return cmd.id != 0 && self.put_virtual(cmd, data, (x, y, w, h));
        }
        // Pixels of the terminal per pixel of the image: the cells asked
        // for, one of them keeping the aspect ratio.
        let (sx, sy) = match (cmd.cols, cmd.rows) {
            (0, 0) => (1.0, 1.0),
            (c, 0) => (c as f64 * cw / w as f64, c as f64 * cw / w as f64),
            (0, r) => (r as f64 * ch / h as f64, r as f64 * ch / h as f64),
            (c, r) => (c as f64 * cw / w as f64, r as f64 * ch / h as f64),
        };
        let (ox, oy) = (cmd.offset.0 as f64, cmd.offset.1 as f64);
        let cols = match cmd.cols {
            0 => ((w as f64 * sx + ox) / cw).ceil() as usize,
            c => c,
        };
        let rows = match cmd.rows {
            0 => ((h as f64 * sy + oy) / ch).ceil() as usize,
            r => r,
        };
        if cmd.placement != 0 {
            let same = Some((cmd.id, cmd.placement));
            self.images.retain(|_, image| image.kitty != same);
        }
        // An image without an id is only placed: it counts as one.
        let bytes = if cmd.id == 0 { data.size() } else { 0 };
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let image = Image {
            data,
            source: (x, y, w, h),
            cell: (cw / sx, ch / sy),
            offset: (ox / sx, oy / sy),
            under: cmd.z < 0,
            alt,
            kitty: (cmd.id != 0).then_some((cmd.id, cmd.placement)),
        };
        let at = self.term.grid().cursor.point;
        let start = self.place(image, bytes, cols, rows, alt);
        let after = match cmd.stay {
            true => format!("\x1b[{};{}H", at.line.0 + 1, at.column.0 + 1),
            false => format!("\x1b[{}G", start + cols + 1),
        };
        self.parser.advance(&mut self.term, after.as_bytes());
        self.drain_history();
        true
    }

    /// Keeps a virtual placement of kitty's image, which its unicode
    /// placeholders show: the image fits the cells asked for, its aspect
    /// ratio kept, centered, as kitty 0.49 draws it.
    fn put_virtual(
        &mut self,
        cmd: &Command,
        data: Arc<Data>,
        source: (usize, usize, usize, usize),
    ) -> bool {
        let (cw, ch) = self.cell;
        let (w, h) = (source.2 as f64, source.3 as f64);
        let cols = if cmd.cols == 0 {
            (w / cw).ceil() as usize
        } else {
            cmd.cols
        };
        let rows = if cmd.rows == 0 {
            (h / ch).ceil() as usize
        } else {
            cmd.rows
        };
        let (bw, bh) = (cols as f64 * cw, rows as f64 * ch);
        // Pixels of the terminal per pixel of the image.
        let s = (bw / w).min(bh / h);
        let image = Image {
            data,
            source,
            cell: (cw / s, ch / s),
            offset: ((bw - w * s) / 2.0 / s, (bh - h * s) / 2.0 / s),
            under: cmd.z < 0,
            alt: false,
            kitty: Some((cmd.id, cmd.placement)),
        };
        self.images.place_virtual(image, cmd.id, cmd.placement);
        // Placeholders already on screen now show it.
        self.full = true;
        true
    }

    /// Deletes kitty's placements, and with an uppercase letter their images
    /// too. False when nothing changed.
    fn delete(&mut self, cmd: &Command) -> bool {
        let id = match cmd.delete {
            b'n' | b'N' => self.images.numbered(cmd.number),
            _ => Some(cmd.id),
        };
        let cursor = self.term.grid().cursor.point;
        let gone: Vec<(u32, u32)> = match cmd.delete {
            b'a' | b'A' => self.shown(|_| true),
            b'c' | b'C' => self.shown(|at| at == cursor),
            b'i' | b'I' | b'n' | b'N' => (self.images.iter())
                .filter(|(_, image)| {
                    matches!(image.kitty, Some((i, p)) if Some(i) == id
                        && (cmd.placement == 0 || p == cmd.placement))
                })
                .map(|(n, _)| n)
                .collect(),
            _ => return false,
        }
        .into_iter()
        .filter_map(|n| Some((n, self.images.get(n)?.kitty?.0)))
        .collect();
        let placed: HashSet<u32> = gone.iter().map(|&(n, _)| n).collect();
        self.images.retain(|n, _| !placed.contains(&n));
        if cmd.delete.is_ascii_uppercase() {
            for i in gone.iter().map(|&(_, i)| i).chain(id) {
                self.images.unstore(i);
            }
        }
        self.full |= !gone.is_empty();
        !gone.is_empty()
    }

    /// The placed images the screen shows, in the cells `at` keeps.
    fn shown(&self, at: impl Fn(alacritty_terminal::index::Point) -> bool) -> HashSet<u32> {
        let grid = self.term.grid();
        let mut shown: HashSet<u32> = (self.overlays.iter())
            .filter(|o| {
                (0..grid.screen_lines()).any(|y| {
                    (0..grid.columns()).any(|x| {
                        o.covers(y as i64, x)
                            && at(alacritty_terminal::index::Point::new(
                                GridLine(y as i32),
                                Column(x),
                            ))
                    })
                })
            })
            .map(|o| o.image)
            .collect();
        for y in 0..grid.screen_lines() {
            let line = GridLine(y as i32);
            for (x, cell) in grid[line][..].iter().enumerate() {
                if let Some(slice) = cell.hyperlink().as_ref().and_then(Slice::of)
                    && at(alacritty_terminal::index::Point::new(line, Column(x)))
                {
                    shown.insert(slice.image);
                }
            }
        }
        shown
    }

    fn parse(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(CHUNK) {
            self.parser.advance(&mut self.term, chunk);
            self.drain_history();
        }
    }

    /// Drops the scrollback, and the images only it showed.
    fn clear_history(&mut self) {
        self.term.grid_mut().clear_history();
        self.history.clear();
        if !self.alt {
            let seen = self.shown(|_| true);
            self.images.retain(|id, _| seen.contains(&id));
        }
        self.full = true;
    }

    /// Drops the images of the alternate screen that it no longer shows.
    fn drop_unseen_alt_images(&mut self) {
        let seen = self.shown(|_| true);
        self.images
            .retain(|id, image| !image.alt || seen.contains(&id));
    }

    /// Moves the rows that scrolled out of the main screen to the scrollback.
    ///
    /// The alternate grid has no history, so this only ever drains the main
    /// screen; lines pushed while the alternate screen is active show up on
    /// exit, which is still the right place in the flow.
    fn drain_history(&mut self) {
        let grid = self.term.grid();
        let rows: Vec<Line> = (1..=grid.history_size() as i32)
            .rev()
            .map(|i| self.render(&grid[GridLine(-i)], None))
            .collect();
        self.term.grid_mut().clear_history();
        for row in rows {
            self.history.push_row(row);
        }
    }

    fn update(&mut self, changed: bool) -> Update {
        let mut update = Update {
            changed,
            ..Update::default()
        };
        for event in self.term_events.try_iter() {
            match event {
                // Sixel, on top of what `alacritty_terminal` announces (VT102).
                TermEvent::PtyWrite(text) if text == "\x1b[?6c" => {
                    update.reply.push_str("\x1b[?62;4;22c")
                }
                TermEvent::PtyWrite(text) => update.reply.push_str(&text),
                // Its callback multiplies a whole cell width, which would drift
                // from the text area over the columns.
                TermEvent::TextAreaSizeRequest(_) => {
                    let (w, h) = self.text_area();
                    update.reply.push_str(&format!("\x1b[4;{h};{w}t"));
                }
                TermEvent::Title(title) => update.title = Some(title),
                TermEvent::ResetTitle => update.title = Some(String::new()),
                _ => {}
            }
        }

        let mode = *self.term.mode();
        let alt = mode.contains(TermMode::ALT_SCREEN);
        if alt != self.alt {
            if !alt {
                self.images.retain(|_, image| !image.alt);
                self.overlays.clear();
            }
            self.alt = alt;
            update.changed = true;
        }
        let modes = Modes::of(&mode);
        if modes != self.modes {
            self.modes = modes;
            update.modes = Some(modes);
        }

        self.drain_history();
        self.stale |= update.changed;
        update
    }

    fn render(&self, row: &GridRow<Cell>, cursor: Option<usize>) -> Line {
        let cells = &row[..];
        let end = cells
            .iter()
            .enumerate()
            .rposition(|(x, cell)| Some(x) == cursor || !is_blank(cell))
            .map_or(0, |x| x + 1);
        let chars: Vec<char> = cells[..end].iter().map(|cell| cell.c).collect();
        let urls = find_urls(&chars);
        let mut runs: Vec<Run> = Vec::new();
        // Building a style allocates: only do it when the attributes change.
        let mut last = None;
        let mut style = Style::default();
        let mut images: Vec<Shown> = Vec::new();
        let mut left: Option<Placeholder> = None;
        // The previous cell when it was a plain one: a plain cell with its
        // attributes only extends its run, which the bulk of a flood is.
        let mut plain: Option<&Cell> = None;
        for (x, cell) in cells[..end].iter().enumerate() {
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let was = plain.take();
            if urls.is_empty()
                && cell.extra.is_none()
                && cell.c != kitty::PLACEHOLDER
                && Some(x) != cursor
            {
                plain = Some(cell);
                if was.is_some_and(|p| (p.fg, p.bg, p.flags) == (cell.fg, cell.bg, cell.flags))
                    && let Some(run) = runs.last_mut().filter(|run| run.cluster == 0)
                {
                    run.text.push(cell.c);
                    continue;
                }
            }
            let url = urls
                .iter()
                .position(|&(start, stop)| (start..stop).contains(&x));
            let link = cell.hyperlink();
            let key = Some((
                cell.fg,
                cell.bg,
                cell.flags,
                Some(x) == cursor,
                link.clone(),
                url,
            ));
            let mark = link.as_ref().and_then(Slice::of);
            let placeholder = (cell.c == kitty::PLACEHOLDER).then(|| {
                let here = Placeholder::of(cell, x, left.filter(|l| l.at + 1 == x));
                left = Some(here);
                here
            });
            let slice = mark.or_else(|| {
                let p = placeholder?;
                let image = self.images.virtual_of(p.id(), p.placement())?;
                Some(Slice {
                    image,
                    row: p.row as u16,
                    col: p.col as u16,
                })
            });
            if let Some(slice) = slice {
                match images.last_mut() {
                    Some(last)
                        if last.at + last.len == x
                            && (last.slice.image, last.slice.row) == (slice.image, slice.row)
                            && last.slice.col as usize + last.len == slice.col as usize =>
                    {
                        last.len += 1
                    }
                    _ => images.push(Shown {
                        at: x,
                        len: 1,
                        slice,
                    }),
                }
            }
            if key != last {
                style = self.style(cell, Some(x) == cursor);
                style.link = link
                    .filter(|_| mark.is_none())
                    .map(|h| h.uri().to_owned())
                    .or_else(|| url.map(|i| chars[urls[i].0..urls[i].1].iter().collect()));
                last = key;
            }
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let joined = runs
                .last()
                .is_some_and(|run| run.cluster > 0 && run.text.ends_with('\u{200d}'));
            if joined {
                let run = runs.last_mut().unwrap();
                run.cluster = run.cluster.saturating_add(width);
            } else if placeholder.is_none() && cell.zerowidth().is_some_and(|z| !z.is_empty()) {
                runs.push(Run {
                    text: String::new(),
                    style: style.clone(),
                    cluster: width,
                });
            } else if runs
                .last()
                .is_none_or(|run| run.cluster > 0 || run.style != style)
            {
                runs.push(Run {
                    text: String::new(),
                    style: style.clone(),
                    cluster: 0,
                });
            }
            let text = &mut runs.last_mut().unwrap().text;
            // Placeholders are blanks for copies and searches.
            if placeholder.is_some() {
                text.push(' ');
                continue;
            }
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
            }
        }
        Line {
            runs,
            wrap: cells
                .last()
                .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE)),
            images,
        }
    }

    fn style(&self, cell: &Cell, cursor: bool) -> Style {
        let flags = cell.flags;
        let fg = match cell.fg {
            Color::Named(n)
                if self.bold_is_bright && flags.contains(Flags::BOLD) && (n as usize) < 8 =>
            {
                Color::Named(n.to_bright())
            }
            Color::Indexed(i) if self.bold_is_bright && flags.contains(Flags::BOLD) && i < 8 => {
                Color::Indexed(i + 8)
            }
            fg => fg,
        };
        let (mut fg, mut bg) = (color(fg), color(cell.bg));
        if flags.contains(Flags::INVERSE) {
            (fg, bg) = (Some(bg.unwrap_or(Tint::Bg)), Some(fg.unwrap_or(Tint::Fg)));
        }
        let attrs = [
            (flags.contains(Flags::BOLD), Attrs::BOLD),
            (flags.contains(Flags::ITALIC), Attrs::ITALIC),
            (flags.intersects(Flags::ALL_UNDERLINES), Attrs::UNDERLINE),
            (flags.contains(Flags::STRIKEOUT), Attrs::STRIKE),
            (flags.contains(Flags::DIM), Attrs::DIM),
            (flags.contains(Flags::HIDDEN), Attrs::HIDDEN),
            (flags.contains(Flags::WIDE_CHAR), Attrs::WIDE),
            (cursor, Attrs::CURSOR),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .fold(Attrs::default(), |all, &(_, a)| all | a);
        Style {
            fg,
            bg,
            attrs,
            link: None,
        }
    }
}

/// A cell of kitty's unicode placeholders: the image id in its foreground
/// color, the placement in its underline color, and its row, column and the
/// high byte of the id in its diacritics, the missing ones taken from the
/// placeholder on its left, as kitty specifies.
#[derive(Clone, Copy)]
struct Placeholder {
    at: usize,
    fg: Color,
    under: Option<Color>,
    row: u32,
    col: u32,
    high: u32,
}

impl Placeholder {
    fn of(cell: &Cell, at: usize, left: Option<Placeholder>) -> Placeholder {
        let numbers: Vec<u32> = (cell.zerowidth().into_iter().flatten())
            .filter_map(|&c| kitty::diacritic(c))
            .collect();
        let (fg, under) = (cell.fg, cell.underline_color());
        let left = left.filter(|l| (l.fg, l.under) == (fg, under));
        let at_left = |row: u32| left.filter(|l| l.row == row);
        let (row, col, high) = match numbers[..] {
            [] => match left {
                Some(l) => (l.row, l.col + 1, l.high),
                None => (0, 0, 0),
            },
            [row] => match at_left(row) {
                Some(l) => (row, l.col + 1, l.high),
                None => (row, 0, 0),
            },
            [row, col] => match at_left(row).filter(|l| l.col + 1 == col) {
                Some(l) => (row, col, l.high),
                None => (row, col, 0),
            },
            [row, col, high, ..] => (row, col, high),
        };
        Placeholder {
            at,
            fg,
            under,
            row,
            col,
            high,
        }
    }

    fn id(&self) -> u32 {
        color_id(self.fg) | self.high << 24
    }

    fn placement(&self) -> u32 {
        self.under.map_or(0, color_id)
    }
}

/// The number a color stands for in placeholders: 24 bits, or 8 in 256
/// colors.
fn color_id(color: Color) -> u32 {
    match color {
        Color::Spec(c) => (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32,
        Color::Indexed(i) => i as u32,
        Color::Named(n) if (n as usize) < 16 => n as u32,
        Color::Named(_) => 0,
    }
}

/// Cell ranges of the http(s) URLs written in a line.
fn find_urls(chars: &[char]) -> Vec<(usize, usize)> {
    let starts_with = |x: usize, prefix: &str| {
        prefix
            .chars()
            .enumerate()
            .all(|(i, c)| chars.get(x + i) == Some(&c))
    };
    let mut found = Vec::new();
    let mut x = 0;
    while x < chars.len() {
        if !starts_with(x, "https://") && !starts_with(x, "http://") {
            x += 1;
            continue;
        }
        let mut end = x;
        while end < chars.len() && !chars[end].is_whitespace() && !"<>\"'`".contains(chars[end]) {
            end += 1;
        }
        // Trailing punctuation usually ends the sentence, not the URL; a
        // closing parenthesis is kept when the URL opened it (wikipedia).
        while end > x && ".,;:!?)]}".contains(chars[end - 1]) {
            let opened = chars[x..end].iter().filter(|&&c| c == '(').count();
            let closed = chars[x..end].iter().filter(|&&c| c == ')').count();
            if chars[end - 1] == ')' && opened >= closed {
                break;
            }
            end -= 1;
        }
        found.push((x, end));
        x = end.max(x + 1);
    }
    found
}

impl Line {
    fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// The cells the runs take.
    pub fn cells(&self) -> usize {
        let cells = |run: &Run| match run.cluster {
            0 if run.style.attrs.has(Attrs::WIDE) => 2 * run.text.chars().count(),
            0 => run.text.chars().count(),
            n => n as usize,
        };
        self.runs.iter().map(cells).sum()
    }
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

fn is_blank(cell: &Cell) -> bool {
    // Most cells have the default background: skip `color` for them.
    cell.c == ' '
        && !cell.flags.contains(Flags::INVERSE)
        && cell.hyperlink().is_none()
        && (cell.bg == Color::Named(NamedColor::Background) || color(cell.bg).is_none())
}

/// The color of a cell, `None` for the default foreground/background.
fn color(color: Color) -> Option<Tint> {
    let rgb = |r: u8, g: u8, b: u8| Tint::Rgb((r as u32) << 16 | (g as u32) << 8 | b as u32);
    match color {
        Color::Named(named) => {
            let named = if (NamedColor::DimBlack..=NamedColor::DimWhite).contains(&named) {
                named.to_bright() // maps dim variants to their normal color
            } else {
                named
            };
            ((named as usize) < 16).then_some(Tint::Palette(named as u8))
        }
        Color::Indexed(i) if i < 16 => Some(Tint::Palette(i)),
        Color::Indexed(i) if i < 232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            let i = i - 16;
            Some(rgb(level(i / 36), level(i / 6 % 6), level(i % 6)))
        }
        Color::Indexed(i) => {
            let v = 8 + 10 * (i - 232);
            Some(rgb(v, v, v))
        }
        Color::Spec(c) => Some(rgb(c.r, c.g, c.b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The view draws the screen as `refresh` keeps it, from the lines
    /// alacritty says changed: after any chunk of the recorded sessions, it
    /// must be the screen rendered whole.
    #[test]
    fn refreshed_screen_is_the_screen_rendered_whole() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "in") {
                continue;
            }
            let input = std::fs::read(&path).unwrap();
            for size in [1, 7, 256] {
                let mut term = Terminal::new(80, 24);
                for (i, chunk) in input.chunks(size).enumerate() {
                    term.advance(chunk);
                    term.refresh();
                    let refreshed = term.screen.clone();
                    (term.full, term.stale) = (true, true);
                    term.refresh();
                    assert!(
                        refreshed == term.screen,
                        "{path:?}, chunk {i} of {size} bytes"
                    );
                }
            }
        }
    }

    /// The texts of the entries of the scrollback, "block" for blocks.
    fn scrollback(term: &Terminal) -> Vec<String> {
        (term.history.first()..term.history.next())
            .map(|n| match term.history.get(n) {
                Some(Entry::Line(line)) => line.text(),
                _ => "block".into(),
            })
            .collect()
    }

    #[test]
    fn clear_drops_the_scrollback_in_either_order() {
        // ncurses 6 (Linux) erases the screen then the scrollback, macOS's
        // `clear` the other way round.
        for clear in [&b"\x1b[H\x1b[2J\x1b[3J"[..], b"\x1b[3J\x1b[H\x1b[2J"] {
            let mut term = Terminal::new(10, 3);
            term.advance(b"a\r\nb\r\nc\r\nd\r\ne");
            term.advance(clear);
            assert!(scrollback(&term).is_empty());
            term.advance(b"f\r\ng\r\nh\r\ni");
            assert_eq!(scrollback(&term), ["f"]);
            // What follows in the same read stays (`clear && make`).
            let mut term = Terminal::new(10, 3);
            term.advance(b"a\r\nb\r\nc\r\nd\r\ne");
            term.advance(&[clear, b"f\r\ng\r\nh\r\ni"].concat());
            assert_eq!(scrollback(&term), ["f"]);
        }
        // Erasing the screen alone keeps it in the scrollback.
        let mut term = Terminal::new(10, 3);
        term.advance(b"a\r\nb\r\nc\r\nd\r\ne\x1b[2J");
        assert_eq!(scrollback(&term), ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn lines_scroll_out_once_in_order() {
        let mut term = Terminal::new(10, 2);
        assert!(term.advance(b"a\r\nb\r\nc\r\nd").changed);
        assert_eq!(scrollback(&term), ["a", "b"]);
        assert_eq!(term.text(), "c\nd\n");
        term.advance(b"e");
        assert_eq!(scrollback(&term), ["a", "b"]);
    }

    #[test]
    fn the_screen_is_rendered_when_read() {
        let mut term = Terminal::new(10, 3);
        term.advance(b"one\r\ntwo");
        assert!(!term.advance(b"").changed);
        assert!(term.advance(b"!").changed);
        assert!(term.screen().is_empty());
        term.refresh();
        // The trailing space is the cursor cell.
        assert_eq!(term.screen()[1].text(), "two! ");
        term.advance(b"?");
        assert_eq!(term.screen()[1].text(), "two! ");
        term.refresh();
        assert_eq!(term.screen()[1].text(), "two!? ");
    }

    #[test]
    fn refreshing_damaged_lines_renders_the_whole_screen() {
        let mut term = Terminal::new(12, 4);
        let steps: [&[u8]; 9] = [
            b"one\r\ntwo\r\nthree",
            b"\x1b[1;1H\x1b[31mX",
            b"\x1b[?25l\x1b[3;5Hhidden",
            b"\x1b[?25h\x1b[2;2H",
            b"\x1b[2K",
            b"\r\n\r\n\r\nscrolled\r\nmore",
            b"\x1b[?1049hALT\x1b[2;3Hb",
            b"\x1b[?1049l\x1b[4;1H\x1b[4h\x1b[1;1Hins",
            b"\x1b[4l\x1b]8;;https://a.b\x1b\\link\x1b]8;;\x1b\\ https://c.d",
        ];
        for step in steps {
            term.advance(step);
            term.refresh();
            let partial: Vec<String> = term
                .screen()
                .iter()
                .map(serde_json::to_string)
                .map(Result::unwrap)
                .collect();
            // Without a previous screen, refresh renders every line.
            term.screen.clear();
            term.stale = true;
            term.refresh();
            let full: Vec<String> = term
                .screen()
                .iter()
                .map(serde_json::to_string)
                .map(Result::unwrap)
                .collect();
            assert_eq!(partial, full, "after {:?}", String::from_utf8_lossy(step));
        }
    }

    #[test]
    fn colors_and_cursor() {
        let mut term = Terminal::new(10, 1);
        term.advance(b"\x1b[1;31mA\x1b[38;5;196mB\x1b[38;2;1;2;3mC\x1b[0m");
        term.refresh();
        let styles: Vec<_> = term.screen()[0].runs.iter().map(|r| &r.style).collect();
        assert_eq!(styles[0].fg, Some(Tint::Palette(1)));
        assert_eq!(styles[0].attrs, Attrs::BOLD);
        assert_eq!(styles[1].fg, Some(Tint::Rgb(0xff0000)));
        assert_eq!(styles[2].fg, Some(Tint::Rgb(0x010203)));
        assert_eq!(styles[3].attrs, Attrs::CURSOR);
        // As the CSS of the first versions, which the snapshots keep.
        let json = serde_json::to_string(styles[0]).unwrap();
        assert_eq!(json, r#"{"fg":"var(--c1)","class":"b"}"#);
    }

    #[test]
    fn alt_screen_round_trip() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"shell");
        assert!(term.advance(b"\x1b[?1049hvim").changed);
        assert!(term.alt_screen());
        term.advance(b"\x1b[?1049l");
        assert!(!term.alt_screen());
        assert_eq!(term.text(), "shell\n\n");
    }

    #[test]
    fn soft_wrap_is_flagged() {
        let mut term = Terminal::new(4, 1);
        term.advance(b"abcdef");
        let Some(Entry::Line(line)) = term.history.get(0) else {
            panic!()
        };
        assert!(line.wrap);
        assert_eq!(line.text(), "abcd");
    }

    #[test]
    fn block_lands_after_output_above_cursor() {
        let mut term = Terminal::new(10, 4);
        // "PGI+" is base64 for "<b>".
        term.advance(b"one\r\ntwo\x1b]7117;html;PGI+\x07$ ");
        assert_eq!(scrollback(&term), ["one", "two", "block"]);
        assert_eq!(term.text(), "$\n\n\n\n");
    }

    #[test]
    fn block_with_known_id_is_replaced_in_place() {
        let mut term = Terminal::new(10, 4);
        term.advance(b"\x1b]7117;html;id=p;PGI+\x07");
        // "PGk+" is base64 for "<i>".
        assert!(term.advance(b"x\x1b]7117;html;id=p;PGk+\x07").changed);
        assert_eq!(scrollback(&term), ["block"]);
        let Some(Entry::Block(block)) = term.history.get(0) else {
            panic!()
        };
        assert_eq!(block.content, "<i>");
        assert_eq!(term.text(), "x\n\n\n\n");
    }

    #[test]
    fn blocks_are_ignored_on_alt_screen() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"\x1b[?1049h\x1b]7117;html;PGI+\x07x");
        assert!(scrollback(&term).is_empty());
        assert_eq!(term.text(), "x\n\n");
    }

    #[test]
    fn mouse_modes_are_reported() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"x");
        let modes = term.advance(b"\x1b[?1002h\x1b[?1006h").modes.unwrap();
        assert_eq!((modes.mouse, modes.sgr_mouse), (2, true));
        assert_eq!(term.advance(b"y").modes, None);
    }

    #[test]
    fn device_status_report_is_answered() {
        let mut term = Terminal::new(10, 2);
        assert_eq!(term.advance(b"ab\x1b[6n").reply, "\x1b[1;3R");
    }

    #[test]
    fn sizes_in_pixels_are_answered_in_order() {
        let mut term = Terminal::new(10, 2);
        term.resize(80, 24, (9.6, 19.0));
        assert_eq!(term.text_area(), (768, 456));
        // As yazi probes: the answers before the DA1 it waits for.
        assert_eq!(
            term.advance(b"\x1b[16t\x1b[14t\x1b[c").reply,
            "\x1b[6;19;10t\x1b[4;456;768t\x1b[?62;4;22c"
        );
    }

    #[test]
    fn bracketed_paste_strips_escapes() {
        let mut term = Terminal::new(10, 2);
        assert_eq!(term.paste("a\nb"), "a\rb");
        term.advance(b"\x1b[?2004h");
        assert_eq!(term.paste("x\x1b[201~y"), "\x1b[200~x[201~y\x1b[201~");
    }

    fn links(term: &mut Terminal, bytes: &[u8]) -> Vec<(String, Option<String>)> {
        term.advance(bytes);
        term.refresh();
        term.screen()[0]
            .runs
            .iter()
            .map(|r| (r.text.clone(), r.style.link.clone()))
            .collect()
    }

    #[test]
    fn urls_in_text_become_links() {
        let mut term = Terminal::new(80, 1);
        let runs = links(
            &mut term,
            b"see https://en.wikipedia.org/wiki/Rust_(language). ok",
        );
        let url = "https://en.wikipedia.org/wiki/Rust_(language)";
        assert_eq!(runs[1], (url.to_owned(), Some(url.to_owned())));
        assert_eq!(runs[2].1, None);
    }

    #[test]
    fn osc8_hyperlinks_become_links() {
        let mut term = Terminal::new(80, 1);
        let runs = links(
            &mut term,
            b"\x1b]8;;https://a.example\x1b\\here\x1b]8;;\x1b\\ x",
        );
        assert_eq!(runs[0], ("here".into(), Some("https://a.example".into())));
    }

    #[test]
    fn bold_is_bright() {
        let mut term = Terminal::new(10, 1);
        term.bold_is_bright = true;
        term.advance(b"\x1b[1;31mA");
        term.refresh();
        assert_eq!(term.screen()[0].runs[0].style.fg, Some(Tint::Palette(9)));
    }

    #[test]
    fn scrollback_entries_are_numbered_and_kept() {
        let mut term = Terminal::new(4, 1);
        term.advance(b"abcdef\r\ng\r\nh");
        // "abcd" soft-wraps: "ef" continues entry 0.
        assert_eq!(scrollback(&term), ["abcdef", "g"]);
        assert_eq!((term.history.first(), term.history.next()), (0, 2));
    }

    #[test]
    fn a_dropped_block_is_inserted_again() {
        let mut term = Terminal::new(10, 2);
        term.history.cap = Some(1);
        term.advance(b"\x1b]7117;html;id=p;PGI+\x07");
        term.advance(b"a\r\nb\r\nc");
        assert_eq!(term.history.block("p"), None);
        term.advance(b"\x1b]7117;html;id=p;PGI+\x07");
        let last = term.history.next() - 1;
        assert_eq!(term.history.block("p"), Some(last));
    }

    fn found(found: Option<Found>) -> Option<(u64, usize)> {
        found.map(|(n, start, _)| (n, start))
    }

    #[test]
    fn search_goes_through_scrollback_and_screen_and_wraps() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"a x\r\nb\r\nx x\r\n$ ");
        term.refresh();
        // Entries 0 "a x" and 1 "b"; the screen is "x x" (2), then "$ " (3).
        let last = found(term.search("X", None, true));
        assert_eq!(last, Some((2, 2)));
        let back = found(term.search("x", last, true));
        assert_eq!(back, Some((2, 0)));
        let back = found(term.search("x", back, true));
        assert_eq!(back, Some((0, 2)));
        assert_eq!(found(term.search("x", back, true)), last);
        assert_eq!(found(term.search("x", last, false)), Some((0, 2)));
        assert_eq!(found(term.search("nope", None, true)), None);
        assert_eq!(found(term.search("", None, true)), None);
    }

    #[test]
    fn search_finds_text_in_blocks() {
        let mut term = Terminal::new(20, 2);
        // "IyBzZWFyY2ggbWU=" is "# search me"; "PGI+Zm91bmQ8L2I+IGEgJmFtcDsgYg==" is
        // "<b>found</b> a &amp; b".
        term.advance(b"\x1b]7117;md;IyBzZWFyY2ggbWU=\x07\x1b]7117;html;PGI+Zm91bmQ8L2I+IGEgJmFtcDsgYg==\x07$ ");
        assert_eq!(found(term.search("me", None, true)), Some((0, 9)));
        assert_eq!(found(term.search("found a &", None, true)), Some((1, 0)));
        assert_eq!(found(term.search("b>", None, true)), None);
    }

    fn units(term: &mut Terminal, bytes: &[u8]) -> Vec<(String, u8)> {
        term.advance(bytes);
        term.refresh();
        term.screen()[0]
            .runs
            .iter()
            .map(|r| (r.text.clone(), r.cluster))
            .collect()
    }

    #[test]
    fn cells_with_combining_chars_are_runs_of_their_own() {
        let mut term = Terminal::new(10, 1);
        let runs = units(&mut term, "ae\u{301}b".as_bytes());
        let expected = [("a", 0), ("e\u{301}", 1), ("b", 0)];
        assert_eq!(runs[..3], expected.map(|(t, n)| (t.to_owned(), n)));
    }

    #[test]
    fn a_joiner_puts_the_next_char_in_the_same_unit() {
        let mut term = Terminal::new(10, 1);
        let runs = units(&mut term, "👩\u{200d}💻x".as_bytes());
        let expected = [("👩\u{200d}💻", 4), ("x", 0)];
        assert_eq!(runs[..2], expected.map(|(t, n)| (t.to_owned(), n)));
    }

    #[test]
    fn programs_change_the_cursor() {
        let mut term = Terminal::new(10, 1);
        term.set_default_cursor(CursorShape::Underline, true);
        assert_eq!(term.cursor(), (CursorShape::Underline, true));
        // As vim in insert mode: a steady bar.
        term.advance(b"\x1b[6 q");
        assert_eq!(term.cursor(), (CursorShape::Beam, false));
        term.advance(b"\x1b[0 q");
        assert_eq!(term.cursor(), (CursorShape::Underline, true));
    }

    /// A red Sixel image of 4 × 12 pixels: 2 × 3 cells of 2 × 4 pixels.
    const SIXEL: &[u8] = b"\x1bPq#1;2;100;0;0#1!4~-!4~\x1b\\";

    fn sixel_term(cols: u16, rows: u16) -> Terminal {
        let mut term = Terminal::new(cols, rows);
        term.resize(cols, rows, (2.0, 4.0));
        term
    }

    /// The slices of a line, with the cell they start at.
    fn slices(line: &Line) -> Vec<(usize, Slice)> {
        line.images.iter().map(|s| (s.at, s.slice)).collect()
    }

    fn slice(row: u16, col: u16) -> Slice {
        Slice { image: 0, row, col }
    }

    #[test]
    fn sixel_marks_its_cells_and_leaves_the_cursor_below() {
        let mut term = sixel_term(10, 5);
        term.advance(b" ");
        term.advance(SIXEL);
        term.advance(b"x");
        term.refresh();
        for y in 0..3 {
            assert_eq!(
                slices(&term.screen()[y]),
                [(1, slice(y as u16, 0))],
                "row {y}"
            );
            assert_eq!(term.screen()[y].text(), "   ");
        }
        // The cursor's cell is in the text.
        assert_eq!(term.screen()[3].text(), " x ");
        assert_eq!(
            term.images.get(0).map(|i| (i.data.width, i.data.height)),
            Some((4, 12))
        );
    }

    #[test]
    fn sixel_scrolls_into_the_history_with_its_cells() {
        // As tall as the screen: the cursor below it scrolls one row out.
        let mut term = sixel_term(10, 3);
        term.advance(SIXEL);
        term.advance(b"x\r\n");
        term.refresh();
        let rows: Vec<_> = (term.history.first()..term.history.next())
            .map(|n| match term.history.get(n) {
                Some(Entry::Line(line)) => slices(line),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(rows, [vec![(0, slice(0, 0))], vec![(0, slice(1, 0))]]);
        assert_eq!(slices(&term.screen()[0]), [(0, slice(2, 0))]);
        assert_eq!(term.screen()[1].text(), "x");
    }

    #[test]
    fn sixel_is_clipped_to_the_text_area() {
        let mut term = sixel_term(1, 2);
        term.advance(SIXEL);
        let image = term.images.get(0).unwrap();
        assert_eq!((image.data.width, image.data.height), (2, 8));
    }

    #[test]
    fn writing_over_a_sixel_cell_cuts_only_that_cell() {
        let mut term = sixel_term(10, 4);
        term.advance(SIXEL);
        term.advance(b"\x1b[1;1Hy");
        term.refresh();
        assert_eq!(slices(&term.screen()[0]), [(1, slice(0, 1))]);
        assert_eq!(slices(&term.screen()[1]), [(0, slice(1, 0))]);
    }

    #[test]
    fn alternate_screen_drops_its_images_on_exit() {
        let mut term = sixel_term(10, 4);
        term.advance(SIXEL);
        term.advance(b"\x1b[?1049h");
        term.advance(SIXEL);
        term.advance(b"\x1b[2J\x1b[H");
        term.advance(SIXEL);
        // The second image was cleared before the third came.
        assert!(term.images.get(1).is_none());
        assert!(term.images.get(2).is_some());
        term.advance(b"\x1b[?1049l");
        assert!(term.images.get(0).is_some());
        assert!(term.images.get(2).is_none());
    }

    #[test]
    fn sixel_is_announced() {
        let mut term = sixel_term(80, 24);
        assert_eq!(
            term.advance(b"\x1b[?1;1;0S\x1b[?2;1;0S\x1b[?3;1;0S\x1b[c")
                .reply,
            "\x1b[?1;0;1024S\x1b[?2;0;160;96S\x1b[?3;1;0S\x1b[?62;4;22c"
        );
    }

    /// As kitty 0.49 answered on Oct 23, 2026, with a cell of 9 × 18 pixels
    /// and 30 rows; kitty's texts of errors are its own.
    #[test]
    fn kitty_places_and_answers_as_kitty() {
        let mut term = Terminal::new(60, 30);
        term.resize(60, 30, (9.0, 18.0));
        let red = |n: usize| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode([255, 0, 0].repeat(n))
        };
        let mut run = |input: String| term.advance(input.as_bytes()).reply;
        assert_eq!(
            run("\x1b[3;5H\x1b_Ga=T,f=24,s=2,v=2,c=4,r=3,i=1;/wAA/wAA/wAA/wAA\x1b\\\x1b[6n".into()),
            "\x1b_Gi=1;OK\x1b\\\x1b[5;9R"
        );
        let big = red(800);
        assert_eq!(
            run(format!(
                "\x1b[3;5H\x1b_Ga=T,f=24,s=20,v=40,i=2,q=1;{big}\x1b\\\x1b[6n"
            )),
            "\x1b[5;8R"
        );
        assert_eq!(
            run("\x1b[3;5H\x1b_Ga=p,i=1,c=4,r=3,C=1,q=1\x1b\\\x1b[6n".into()),
            "\x1b[3;5R"
        );
        assert_eq!(
            run("\x1b[50;5H\x1b_Ga=p,i=1,c=4,r=3,q=1\x1b\\\x1b[6n".into()),
            "\x1b[30;9R"
        );
        assert_eq!(
            run("\x1b_Ga=p,i=99\x1b\\".into()),
            "\x1b_Gi=99;ENOENT:no image with this id or number\x1b\\"
        );
        let numbered = run("\x1b_Ga=t,f=24,s=2,v=2,I=7;/wAA/wAA/wAA/wAA\x1b\\".into());
        assert!(numbered.starts_with("\x1b_Gi=") && numbered.ends_with(",I=7;OK\x1b\\"));
        assert_eq!(
            run("\x1b_Ga=t,f=24,s=3,v=2,i=5;/wAA/wAA/wAA/wAA\x1b\\".into()),
            "\x1b_Gi=5;ENODATA:insufficient image data\x1b\\"
        );
        assert_eq!(
            run("\x1b_Ga=t,t=s,f=24,s=1,v=1,i=6;L3NobQ==\x1b\\".into()),
            "\x1b_Gi=6;EINVAL:only direct transmission is supported\x1b\\"
        );
        assert_eq!(
            run("\x1b_Ga=q,i=31,s=1,v=1,t=d,f=24;AAAA\x1b\\".into()),
            "\x1b_Gi=31;OK\x1b\\"
        );
        assert_eq!(run("\x1b_Ga=d,d=i,i=1\x1b_Ga=x,i=32\x1b\\".into()), "");
    }

    #[test]
    fn placeholders_show_virtual_placements() {
        let mut term = sixel_term(30, 6);
        // A 2 × 2 image in 8 × 2 cells of 2 × 4 pixels: centered, a pixel of
        // the image (4 of the terminal) on each side.
        term.advance(b"\x1b_Ga=T,f=24,s=2,v=2,c=8,r=2,U=1,i=42,q=2;/wAA/wAA/wAA/wAA\x1b\\");
        let placed = term.images.virtual_of(42, 0).unwrap();
        let image = term.images.get(placed).unwrap();
        assert_eq!((image.cell, image.offset), ((0.5, 1.0), (1.0, 0.0)));
        // Rows named on the first cell only, then all diacritics and the
        // id in 24 bits, with its high byte (0) in a third diacritic.
        let (ph, d0, d1, d2) = ("\u{10eeee}", "\u{305}", "\u{30d}", "\u{30e}");
        let row0 = format!("\x1b[38;5;42m{ph}{d0}{ph}{ph}\x1b[39m|\r\n");
        let row1 = format!("\x1b[38;2;0;0;42mx{ph}{d1}{d2}{d0}{ph}\x1b[39m\r\n");
        term.advance(format!("{row0}{row1}").as_bytes());
        term.refresh();
        let slice = |row, col| Slice {
            image: placed,
            row,
            col,
        };
        assert_eq!(slices(&term.screen()[0]), [(0, slice(0, 0))]);
        assert_eq!(term.screen()[0].images[0].len, 3);
        assert_eq!(slices(&term.screen()[1]), [(1, slice(1, 2))]);
        assert_eq!(term.screen()[1].images[0].len, 2);
        // Blanks for copies and searches.
        assert_eq!(term.screen()[0].text(), "   |");
        // Another id shows nothing.
        term.advance(format!("\x1b[38;5;41m{ph}{d0}{d0}\x1b[39m").as_bytes());
        term.refresh();
        assert_eq!(slices(&term.screen()[2]), []);
    }

    /// As ratune draws its cover: the image, then the text of the frame
    /// over it, which leaves it shown in kitty 0.49.
    #[test]
    fn kitty_images_of_the_alternate_screen_survive_text_over_them() {
        let mut term = Terminal::new(20, 10);
        term.resize(20, 10, (2.0, 4.0));
        let put = "\x1b[3;5H\x1b_Ga=T,f=24,s=1,v=1,c=4,r=2,i=1,q=2;/wAA\x1b\\";
        let shown = |term: &mut Terminal| {
            term.refresh();
            (0..10)
                .map(|y| slices(&term.screen()[y]).len())
                .collect::<Vec<_>>()
        };
        term.advance(format!("\x1b[?1049h{put}").as_bytes());
        assert_eq!(shown(&mut term), [0, 0, 1, 1, 0, 0, 0, 0, 0, 0]);
        term.advance(b"\x1b[3;1H                    \x1b[4;1H                    ");
        assert_eq!(shown(&mut term), [0, 0, 1, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(slices(&term.screen()[3])[0].0, 4);
        // Cleared with the screen, by kitty's deletion, and on exit.
        term.advance(b"\x1b[2J");
        assert_eq!(shown(&mut term), [0; 10]);
        term.advance(put.as_bytes());
        term.advance(b"\x1b_Ga=d,d=a,q=2\x1b\\");
        assert_eq!(shown(&mut term), [0; 10]);
        term.advance(put.as_bytes());
        term.advance(b"\x1b[?1049l");
        assert_eq!(shown(&mut term), [0; 10]);
        // On the main screen, text erases them as Sixel images.
        term.advance(format!("{put}\x1b[3;5Hxxxx").as_bytes());
        assert_eq!(shown(&mut term)[2], 0);
    }

    #[test]
    fn kitty_images_of_the_alternate_screen_scroll_with_it() {
        let mut term = Terminal::new(20, 4);
        term.resize(20, 4, (2.0, 4.0));
        // Two rows from the last one: the screen scrolls one row.
        term.advance(b"\x1b[?1049h\x1b[4;1H\x1b_Ga=T,f=24,s=1,v=1,c=2,r=2,i=1,q=2;/wAA\x1b\\");
        term.refresh();
        let rows: Vec<usize> = (0..4).map(|y| slices(&term.screen()[y]).len()).collect();
        assert_eq!(rows, [0, 0, 1, 1]);
    }

    #[test]
    fn kitty_deletes_placements_and_images() {
        let mut term = Terminal::new(20, 10);
        term.resize(20, 10, (2.0, 4.0));
        let put = "\x1b_Ga=T,f=24,s=1,v=1,c=2,r=1,i=4,p=1,q=2;/wAA\x1b\\";
        term.advance(put.as_bytes());
        // The same placement again replaces it.
        term.advance(format!("\r\n{put}").as_bytes());
        term.advance(b"\r\n\x1b_Ga=p,i=4,p=2,q=2\x1b\\");
        let placed = |term: &Terminal| term.images.iter().count();
        assert_eq!(placed(&term), 2);
        term.advance(b"\x1b_Ga=d,d=i,i=4,p=2\x1b\\");
        assert_eq!(placed(&term), 1);
        assert!(term.images.stored(4).is_some());
        term.advance(b"\x1b_Ga=d,d=I,i=4\x1b\\");
        assert_eq!(placed(&term), 0);
        assert!(term.images.stored(4).is_none());
        assert_eq!(term.images.removed, 3);
    }

    #[test]
    fn title_is_forwarded() {
        let mut term = Terminal::new(10, 1);
        assert_eq!(
            term.advance(b"\x1b]0;vim\x07").title.as_deref(),
            Some("vim")
        );
        assert_eq!(term.advance(b"x").title, None);
    }
}
