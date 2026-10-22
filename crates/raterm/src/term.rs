use std::sync::mpsc::{Receiver, Sender, channel};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Row as GridRow};
use alacritty_terminal::index::Line as GridLine;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
pub use alacritty_terminal::vte::ansi::CursorShape;
use alacritty_terminal::vte::ansi::{Color, CursorStyle, NamedColor, Processor};
use serde::Serialize;

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

pub struct Terminal {
    /// Bold text in one of the 8 normal colors uses its bright variant.
    pub bold_is_bright: bool,
    term: Term<Listener>,
    parser: Processor,
    term_events: Receiver<TermEvent>,
    filter: Filter,
    /// Local files shown by video blocks.
    pub media: Media,
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
        let term = Term::new(config, &size, Listener(tx));
        Terminal {
            bold_is_bright: false,
            // The view starts from the same modes.
            modes: Modes::of(term.mode()),
            term,
            parser: Processor::new(),
            term_events: rx,
            filter: Filter::default(),
            media: Media::default(),
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
                    for chunk in bytes.chunks(CHUNK) {
                        self.parser.advance(&mut self.term, chunk);
                        self.drain_history();
                    }
                    // Rendering the screen to know whether it changed would
                    // cost what rendering it lazily saves.
                    changed = true;
                }
                Piece::Block(block) => changed |= self.insert(block),
            }
        }
        self.update(changed)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Update {
        self.term
            .resize(TermSize::new(cols.max(1) as usize, rows.max(1) as usize));
        self.update(true)
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
        let mut screen = std::mem::take(&mut self.screen);
        let grid = self.term.grid();
        let cursor = self
            .term
            .mode()
            .contains(TermMode::SHOW_CURSOR)
            .then_some(grid.cursor.point);
        let render = |y: usize| {
            let x = cursor.filter(|c| c.line.0 == y as i32).map(|c| c.column.0);
            self.render(&grid[GridLine(y as i32)], x)
        };
        match damaged {
            Some(lines) if screen.len() == grid.screen_lines() => {
                for y in lines {
                    screen[y] = render(y);
                }
            }
            _ => screen = (0..grid.screen_lines()).map(render).collect(),
        }
        self.screen = screen;
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
                TermEvent::PtyWrite(text) => update.reply.push_str(&text),
                TermEvent::Title(title) => update.title = Some(title),
                TermEvent::ResetTitle => update.title = Some(String::new()),
                _ => {}
            }
        }

        let mode = *self.term.mode();
        let alt = mode.contains(TermMode::ALT_SCREEN);
        if alt != self.alt {
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
        for (x, cell) in cells[..end].iter().enumerate() {
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let url = urls
                .iter()
                .position(|&(start, stop)| (start..stop).contains(&x));
            let key = Some((
                cell.fg,
                cell.bg,
                cell.flags,
                Some(x) == cursor,
                cell.hyperlink(),
                url,
            ));
            if key != last {
                style = self.style(cell, Some(x) == cursor);
                style.link = cell
                    .hyperlink()
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
            } else if cell.zerowidth().is_some_and(|z| !z.is_empty()) {
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
            text.push(cell.c);
            text.extend(cell.zerowidth().into_iter().flatten());
        }
        Line {
            runs,
            wrap: cells
                .last()
                .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE)),
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
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

fn is_blank(cell: &Cell) -> bool {
    // Most cells have the default background: skip `color` for them.
    cell.c == ' '
        && !cell.flags.contains(Flags::INVERSE)
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
