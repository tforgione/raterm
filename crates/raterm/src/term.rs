use std::sync::mpsc::{Receiver, Sender, channel};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Row as GridRow};
use alacritty_terminal::index::Line as GridLine;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use serde::Serialize;

use crate::history::{self, Entry, History, Numbered, WINDOW};
use crate::media::Media;
use crate::rich::{Block, Filter, Piece, Rendered};

/// History is drained every `CHUNK` bytes, so it only needs to hold the lines
/// that can scroll out meanwhile (one per byte, more with `CSI S`).
const HISTORY: usize = 1 << 16;

/// Past 1,000 rows of history, alacritty frees the rows it drops, and
/// allocates them again as the next lines scroll out: draining at least that
/// often keeps them for reuse.
const CHUNK: usize = 1000;

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Rows of the visible screen that changed, and its current height.
    Lines {
        height: usize,
        rows: Vec<Row>,
    },
    /// A row that left the main screen by the top, to append to the
    /// scrollback as `entry`; a soft-wrapped line continues the same entry.
    ScrollOut {
        entry: u64,
        line: Line,
    },
    Alt {
        on: bool,
    },
    /// Modes that change how the view encodes input.
    Mode(Modes),
    /// Window title set by the program (OSC 0/2); empty to reset it.
    Title {
        title: String,
    },
    /// A rich block, inserted in the flow before the screen as `entry`, or
    /// replacing that entry, a block with the same id.
    Block {
        entry: u64,
        #[serde(flatten)]
        block: Rendered,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
    },
    /// Entries kept in the scrollback: `first..next`.
    History {
        first: u64,
        next: u64,
    },
    /// Entries of the scrollback the page asked for.
    Entries {
        entries: Vec<Numbered>,
    },
    /// Next match of a search: an entry of the scrollback, or `next + y` for
    /// row `y` of the screen; `None` when there is none.
    Found {
        entry: Option<u64>,
        start: usize,
        end: usize,
    },
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Default)]
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

#[derive(Serialize, Debug, PartialEq)]
pub struct Row {
    pub y: usize,
    #[serde(flatten)]
    pub line: Line,
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub runs: Vec<Run>,
    /// The line continues on the next one (soft wrap).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wrap: bool,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    #[serde(flatten)]
    pub style: Style,
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct Style {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub class: String,
    /// Target of an OSC 8 hyperlink, or of a URL written in the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

struct Listener(Sender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.send(event);
    }
}

#[derive(Debug, Default)]
pub struct Update {
    pub events: Vec<Event>,
    /// Bytes the terminal answers to the program (e.g. device status reports).
    pub reply: String,
}

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
    /// Range of the history as last sent to the view.
    sent_history: (u64, u64),
    /// Screen as last sent to the view.
    screen: Vec<Line>,
    alt: bool,
    /// Modes as last sent to the view.
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
            // The view starts from the same modes (see app.js).
            modes: Modes::of(term.mode()),
            term,
            parser: Processor::new(),
            term_events: rx,
            filter: Filter::default(),
            media: Media::default(),
            history: History::default(),
            sent_history: (0, 0),
            screen: Vec::new(),
            alt: false,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Update {
        let mut events = Vec::new();
        for piece in self.filter.feed(bytes) {
            match piece {
                Piece::Bytes(bytes) => {
                    for chunk in bytes.chunks(CHUNK) {
                        self.parser.advance(&mut self.term, chunk);
                        self.drain_history(&mut events);
                    }
                }
                Piece::Block(block) => self.insert(block, &mut events),
            }
        }
        self.update(events)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Update {
        self.term
            .resize(TermSize::new(cols.max(1) as usize, rows.max(1) as usize));
        self.update(Vec::new())
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

    /// The screen as last rendered: the alternate one while it is active.
    pub fn screen(&self) -> &[Line] {
        &self.screen
    }

    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// Entries `from..from + count` of the scrollback.
    pub fn entries(&self, from: u64, count: usize) -> Event {
        Event::Entries {
            entries: self.history.slice(from, count),
        }
    }

    /// The match of `query` next to `from` (an entry and an offset in its
    /// text, see `Event::Found`), going backwards or forwards over the lines of
    /// the scrollback and of the screen, and wrapping around. Without `from`,
    /// starts from the end, or from the beginning forwards.
    pub fn search(&self, query: &str, from: Option<(u64, usize)>, backwards: bool) -> Event {
        let needle = query.to_lowercase();
        let (first, next) = (self.history.first(), self.history.next());
        let end = next + self.screen.len() as u64;
        let text = |n: u64| match self.history.get(n) {
            Some(Entry::Line(line)) => Some(line.text()),
            Some(Entry::Block(_)) => None,
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
                return Event::Found {
                    entry: Some(first + i),
                    start,
                    end,
                };
            }
        }
        Event::Found {
            entry: None,
            start: 0,
            end: 0,
        }
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
    fn insert(&mut self, block: Block, events: &mut Vec<Event>) {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let old = block.id.as_ref().and_then(|id| self.history.block(id));
        let block = match block.render(&self.media) {
            Ok(block) => block,
            Err(e) => return eprintln!("raterm: invalid rich block: {e}"),
        };
        if let Some(entry) = old {
            self.history.replace(entry, Entry::Block(block.clone()));
            return events.push(Event::Block {
                entry,
                block,
                replace: true,
            });
        }
        if self.term.grid().cursor.point.column.0 > 0 {
            self.parser.advance(&mut self.term, b"\r\n");
        }
        let y = self.term.grid().cursor.point.line.0;
        if y > 0 {
            let scroll = format!("\x1b[{y}S\x1b[H");
            self.parser.advance(&mut self.term, scroll.as_bytes());
        }
        self.drain_history(events);
        let entry = self.history.push(Entry::Block(block.clone()));
        events.push(Event::Block {
            entry,
            block,
            replace: false,
        });
    }

    /// Moves the rows that scrolled out of the main screen to the scrollback,
    /// and emits them.
    ///
    /// The alternate grid has no history, so this only ever drains the main
    /// screen; lines pushed while the alternate screen is active show up on
    /// exit, which is still the right place in the flow.
    fn drain_history(&mut self, events: &mut Vec<Event>) {
        let grid = self.term.grid();
        let rows: Vec<Line> = (1..=grid.history_size() as i32)
            .rev()
            .map(|i| self.render(&grid[GridLine(-i)], None))
            .collect();
        self.term.grid_mut().clear_history();
        events.extend(rows.into_iter().map(|line| Event::ScrollOut {
            entry: self.history.push_row(line.clone()),
            line,
        }));
    }

    fn update(&mut self, events: Vec<Event>) -> Update {
        let mut update = Update {
            events,
            reply: String::new(),
        };
        for event in self.term_events.try_iter() {
            match event {
                TermEvent::PtyWrite(text) => update.reply.push_str(&text),
                TermEvent::Title(title) => update.events.push(Event::Title { title }),
                TermEvent::ResetTitle => update.events.push(Event::Title {
                    title: String::new(),
                }),
                _ => {}
            }
        }

        let mode = *self.term.mode();
        let alt = mode.contains(TermMode::ALT_SCREEN);
        if alt != self.alt {
            self.alt = alt;
            self.screen.clear();
            update.events.push(Event::Alt { on: alt });
        }
        let modes = Modes::of(&mode);
        if modes != self.modes {
            self.modes = modes;
            update.events.push(Event::Mode(modes));
        }

        self.drain_history(&mut update.events);
        trim(&mut update.events);

        let grid = self.term.grid();
        let cursor = mode
            .contains(TermMode::SHOW_CURSOR)
            .then_some(grid.cursor.point);
        let height = grid.screen_lines();
        let mut rows = Vec::new();
        let screen: Vec<Line> = (0..height)
            .map(|y| {
                let x = cursor.filter(|c| c.line.0 == y as i32).map(|c| c.column.0);
                let line = self.render(&grid[GridLine(y as i32)], x);
                if self.screen.get(y) != Some(&line) {
                    rows.push(Row {
                        y,
                        line: line.clone(),
                    });
                }
                line
            })
            .collect();
        if !rows.is_empty() || height != self.screen.len() {
            update.events.push(Event::Lines { height, rows });
        }
        self.screen = screen;
        let range = (self.history.first(), self.history.next());
        if range != self.sent_history {
            self.sent_history = range;
            let (first, next) = range;
            update.events.push(Event::History { first, next });
        }
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
                let mut style = self.style(cell, Some(x) == cursor);
                style.link = cell
                    .hyperlink()
                    .map(|h| h.uri().to_owned())
                    .or_else(|| url.map(|i| chars[urls[i].0..urls[i].1].iter().collect()));
                if runs.last().is_none_or(|run| run.style != style) {
                    runs.push(Run {
                        text: String::new(),
                        style,
                    });
                }
                last = key;
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
            (fg, bg) = (
                Some(bg.unwrap_or("var(--bg)".into())),
                Some(fg.unwrap_or("var(--fg)".into())),
            );
        }
        let class = [
            (flags.contains(Flags::BOLD), "b"),
            (flags.contains(Flags::ITALIC), "i"),
            (flags.intersects(Flags::ALL_UNDERLINES), "u"),
            (flags.contains(Flags::STRIKEOUT), "s"),
            (flags.contains(Flags::DIM), "d"),
            (flags.contains(Flags::HIDDEN), "h"),
            // Each character takes two cells (CJK, some emoji).
            (flags.contains(Flags::WIDE_CHAR), "wide"),
            // Drawn by the view, according to the configured cursor style.
            (cursor, "cursor"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(" ");
        Style {
            fg,
            bg,
            class,
            link: None,
        }
    }
}

/// Leaves out the entries that `events` add to the scrollback before the last
/// `WINDOW` of them: the page would drop them before showing them.
pub fn trim(events: &mut Vec<Event>) {
    let added = |e: &Event| match *e {
        Event::ScrollOut { entry, .. }
        | Event::Block {
            entry,
            replace: false,
            ..
        } => Some(entry),
        _ => None,
    };
    let Some(newest) = events.iter().filter_map(added).max() else {
        return;
    };
    events.retain(|e| added(e).is_none_or(|entry| entry + WINDOW as u64 > newest));
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

fn is_blank(cell: &Cell) -> bool {
    // Most cells have the default background: skip `color` for them.
    cell.c == ' '
        && !cell.flags.contains(Flags::INVERSE)
        && (cell.bg == Color::Named(NamedColor::Background) || color(cell.bg).is_none())
}

/// CSS color for a cell color, `None` for the default foreground/background.
fn color(color: Color) -> Option<String> {
    match color {
        Color::Named(named) => {
            let named = if (NamedColor::DimBlack..=NamedColor::DimWhite).contains(&named) {
                named.to_bright() // maps dim variants to their normal color
            } else {
                named
            };
            ((named as usize) < 16).then(|| format!("var(--c{})", named as usize))
        }
        Color::Indexed(i) if i < 16 => Some(format!("var(--c{i})")),
        Color::Indexed(i) if i < 232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            let i = i - 16;
            Some(hex(level(i / 36), level(i / 6 % 6), level(i % 6)))
        }
        Color::Indexed(i) => {
            let v = 8 + 10 * (i - 232);
            Some(hex(v, v, v))
        }
        Color::Spec(rgb) => Some(hex(rgb.r, rgb.g, rgb.b)),
    }
}

fn hex(r: u8, g: u8, b: u8) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(7);
    s.push('#');
    for v in [r, g, b] {
        s.push(DIGITS[(v >> 4) as usize] as char);
        s.push(DIGITS[(v & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrolled(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::ScrollOut { line, .. } => Some(line.text()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lines_scroll_out_once_in_order() {
        let mut term = Terminal::new(10, 2);
        let update = term.advance(b"a\r\nb\r\nc\r\nd");
        assert_eq!(scrolled(&update.events), ["a", "b"]);
        assert_eq!(term.text(), "c\nd\n");
        assert!(scrolled(&term.advance(b"e").events).is_empty());
    }

    #[test]
    fn only_changed_rows_are_sent() {
        let mut term = Terminal::new(10, 3);
        term.advance(b"one\r\ntwo");
        let update = term.advance(b"!");
        let [Event::Lines { rows, .. }] = &update.events[..] else {
            panic!("{:?}", update.events)
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].y, 1);
        // The trailing space is the cursor cell.
        assert_eq!(rows[0].line.text(), "two! ");
    }

    #[test]
    fn colors_and_cursor() {
        let mut term = Terminal::new(10, 1);
        let update = term.advance(b"\x1b[1;31mA\x1b[38;5;196mB\x1b[38;2;1;2;3mC\x1b[0m");
        let [Event::Lines { rows, .. }] = &update.events[..] else {
            panic!()
        };
        let styles: Vec<_> = rows[0].line.runs.iter().map(|r| &r.style).collect();
        assert_eq!(styles[0].fg.as_deref(), Some("var(--c1)"));
        assert_eq!(styles[0].class, "b");
        assert_eq!(styles[1].fg.as_deref(), Some("#ff0000"));
        assert_eq!(styles[2].fg.as_deref(), Some("#010203"));
        assert_eq!(styles[3].class, "cursor");
    }

    #[test]
    fn alt_screen_round_trip() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"shell");
        let on = term.advance(b"\x1b[?1049hvim");
        assert_eq!(on.events[0], Event::Alt { on: true });
        let off = term.advance(b"\x1b[?1049l");
        assert_eq!(off.events[0], Event::Alt { on: false });
        assert_eq!(term.text(), "shell\n\n");
    }

    #[test]
    fn soft_wrap_is_flagged() {
        let mut term = Terminal::new(4, 1);
        let update = term.advance(b"abcdef");
        let [Event::ScrollOut { line, .. }, ..] = &update.events[..] else {
            panic!()
        };
        assert!(line.wrap);
        assert_eq!(line.text(), "abcd");
    }

    fn kinds(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::ScrollOut { line, .. } => Some(line.text()),
                Event::Block { replace, .. } => Some(format!("block {replace}")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn block_lands_after_output_above_cursor() {
        let mut term = Terminal::new(10, 4);
        // "PGI+" is base64 for "<b>".
        let update = term.advance(b"one\r\ntwo\x1b]7117;html;PGI+\x07$ ");
        assert_eq!(kinds(&update.events), ["one", "two", "block false"]);
        assert_eq!(term.text(), "$\n\n\n\n");
    }

    #[test]
    fn block_with_known_id_is_replaced_in_place() {
        let mut term = Terminal::new(10, 4);
        term.advance(b"\x1b]7117;html;id=p;PGI+\x07");
        let update = term.advance(b"x\x1b]7117;html;id=p;PGI+\x07");
        assert_eq!(kinds(&update.events), ["block true"]);
        assert_eq!(term.text(), "x\n\n\n\n");
    }

    #[test]
    fn blocks_are_ignored_on_alt_screen() {
        let mut term = Terminal::new(10, 2);
        let update = term.advance(b"\x1b[?1049h\x1b]7117;html;PGI+\x07x");
        assert_eq!(kinds(&update.events), Vec::<String>::new());
        assert_eq!(term.text(), "x\n\n");
    }

    #[test]
    fn mouse_modes_are_reported() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"x");
        let update = term.advance(b"\x1b[?1002h\x1b[?1006h");
        let [Event::Mode(modes), ..] = &update.events[..] else {
            panic!("{:?}", update.events)
        };
        assert_eq!((modes.mouse, modes.sgr_mouse), (2, true));
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
        let update = term.advance(bytes);
        let Some(Event::Lines { rows, .. }) = update.events.last() else {
            panic!()
        };
        rows[0]
            .line
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
        let update = term.advance(b"\x1b[1;31mA");
        let [Event::Lines { rows, .. }] = &update.events[..] else {
            panic!()
        };
        assert_eq!(rows[0].line.runs[0].style.fg.as_deref(), Some("var(--c9)"));
    }

    #[test]
    fn scrollback_entries_are_numbered_and_kept() {
        let mut term = Terminal::new(4, 1);
        let update = term.advance(b"abcdef\r\ng\r\nh");
        let entries: Vec<u64> = update
            .events
            .iter()
            .filter_map(|e| match e {
                Event::ScrollOut { entry, .. } => Some(*entry),
                _ => None,
            })
            .collect();
        // "abcd" soft-wraps: "ef" continues entry 0.
        assert_eq!(entries, [0, 0, 1]);
        assert_eq!(
            update.events.last(),
            Some(&Event::History { first: 0, next: 2 })
        );
        let Event::Entries { entries } = term.entries(0, 10) else {
            panic!()
        };
        let texts: Vec<_> = entries
            .iter()
            .map(|e| match &e.item {
                Entry::Line(line) => line.text(),
                Entry::Block(_) => panic!(),
            })
            .collect();
        assert_eq!(texts, ["abcdef", "g"]);
    }

    #[test]
    fn a_flood_sends_only_the_last_window() {
        let mut term = Terminal::new(10, 1);
        let flood = "x\r\n".repeat(WINDOW + 10);
        let update = term.advance(flood.as_bytes());
        let sent: Vec<u64> = update
            .events
            .iter()
            .filter_map(|e| match e {
                Event::ScrollOut { entry, .. } => Some(*entry),
                _ => None,
            })
            .collect();
        assert_eq!(sent.len(), WINDOW);
        assert_eq!(sent[0], 10);
        assert_eq!(term.history.next(), (WINDOW + 10) as u64);
    }

    #[test]
    fn batches_put_together_keep_the_last_window() {
        let mut term = Terminal::new(10, 1);
        let mut events = term.advance(b"a\r\n").events;
        events.extend(term.advance("x\r\n".repeat(WINDOW).as_bytes()).events);
        trim(&mut events);
        assert_eq!(scrolled(&events).len(), WINDOW);
        assert!(scrolled(&events).iter().all(|text| text == "x"));
        assert!(events.iter().any(|e| matches!(e, Event::History { .. })));
    }

    #[test]
    fn a_dropped_block_is_inserted_again() {
        let mut term = Terminal::new(10, 2);
        term.history.cap = Some(1);
        term.advance(b"\x1b]7117;html;id=p;PGI+\x07");
        term.advance(b"a\r\nb\r\nc");
        let update = term.advance(b"\x1b]7117;html;id=p;PGI+\x07");
        assert!(matches!(
            update.events[..],
            [.., Event::Block { replace: false, .. }, _, _]
        ));
    }

    fn found(event: Event) -> Option<(u64, usize)> {
        match event {
            Event::Found { entry, start, .. } => entry.map(|n| (n, start)),
            _ => panic!(),
        }
    }

    #[test]
    fn search_goes_through_scrollback_and_screen_and_wraps() {
        let mut term = Terminal::new(10, 2);
        term.advance(b"a x\r\nb\r\nx x\r\n$ ");
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
    fn title_is_forwarded() {
        let mut term = Terminal::new(10, 1);
        let update = term.advance(b"\x1b]0;vim\x07");
        assert_eq!(
            update.events[0],
            Event::Title {
                title: "vim".into()
            }
        );
    }
}
