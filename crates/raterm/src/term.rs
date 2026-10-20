use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Row as GridRow};
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use serde::Serialize;

use crate::media::Media;
use crate::rich::{Block, Filter, Piece, Rendered};

/// History is drained after every batch, so it only needs to hold the lines
/// that can scroll out during one batch (at most one per input byte).
const HISTORY: usize = 1 << 16;

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Rows of the visible screen that changed, and its current height.
    Lines {
        height: usize,
        rows: Vec<Row>,
    },
    /// A line that left the main screen by the top, to append to the scrollback.
    ScrollOut {
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
    /// A rich block, inserted in the flow before the screen, or replacing the
    /// block with the same id.
    Block {
        #[serde(flatten)]
        block: Rendered,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
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
    /// Ids of the rich blocks inserted so far.
    ids: HashSet<String>,
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
            ids: HashSet::new(),
            screen: Vec::new(),
            alt: false,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Update {
        let mut events = Vec::new();
        for piece in self.filter.feed(bytes) {
            match piece {
                Piece::Bytes(bytes) => self.parser.advance(&mut self.term, &bytes),
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
        let replace = block
            .id
            .as_ref()
            .is_some_and(|id| !self.ids.insert(id.clone()));
        let block = match block.render(&self.media) {
            Ok(block) => block,
            Err(e) => return eprintln!("raterm: invalid rich block: {e}"),
        };
        if !replace {
            if self.term.grid().cursor.point.column.0 > 0 {
                self.parser.advance(&mut self.term, b"\r\n");
            }
            let y = self.term.grid().cursor.point.line.0;
            if y > 0 {
                let scroll = format!("\x1b[{y}S\x1b[H");
                self.parser.advance(&mut self.term, scroll.as_bytes());
            }
            self.drain_history(events);
        }
        events.push(Event::Block { block, replace });
    }

    /// Emits the lines that scrolled out of the main screen.
    ///
    /// The alternate grid has no history, so this only ever drains the main
    /// screen; lines pushed while the alternate screen is active show up on
    /// exit, which is still the right place in the flow.
    fn drain_history(&mut self, events: &mut Vec<Event>) {
        let grid = self.term.grid();
        for i in (1..=grid.history_size() as i32).rev() {
            let line = self.render(&grid[GridLine(-i)], None);
            events.push(Event::ScrollOut { line });
        }
        self.term.grid_mut().clear_history();
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
        update
    }

    fn render(&self, row: &GridRow<Cell>, cursor: Option<usize>) -> Line {
        let cols = row.len();
        let end = (0..cols)
            .rev()
            .find(|&x| Some(x) == cursor || !is_blank(&row[Column(x)]))
            .map_or(0, |x| x + 1);
        let chars: Vec<char> = (0..end).map(|x| row[Column(x)].c).collect();
        let mut urls = vec![None; end];
        for (start, stop) in find_urls(&chars) {
            let url: String = chars[start..stop].iter().collect();
            urls[start..stop].fill(Some(url));
        }
        let mut runs: Vec<Run> = Vec::new();
        for (x, url) in urls.into_iter().enumerate() {
            let cell = &row[Column(x)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let mut style = self.style(cell, Some(x) == cursor);
            style.link = cell.hyperlink().map(|h| h.uri().to_owned()).or(url);
            let mut text = String::from(cell.c);
            text.extend(cell.zerowidth().into_iter().flatten());
            match runs.last_mut() {
                Some(run) if run.style == style => run.text.push_str(&text),
                _ => runs.push(Run { text, style }),
            }
        }
        Line {
            runs,
            wrap: row[Column(cols - 1)].flags.contains(Flags::WRAPLINE),
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
    cell.c == ' ' && color(cell.bg).is_none() && !cell.flags.contains(Flags::INVERSE)
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
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrolled(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::ScrollOut { line } => Some(line.text()),
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
        let [Event::ScrollOut { line }, ..] = &update.events[..] else {
            panic!()
        };
        assert!(line.wrap);
        assert_eq!(line.text(), "abcd");
    }

    fn kinds(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::ScrollOut { line } => Some(line.text()),
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
