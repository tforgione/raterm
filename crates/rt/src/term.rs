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
    Mode {
        app_cursor: bool,
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
    term: Term<Listener>,
    parser: Processor,
    term_events: Receiver<TermEvent>,
    filter: Filter,
    /// Ids of the rich blocks inserted so far.
    ids: HashSet<String>,
    /// Screen as last sent to the view.
    screen: Vec<Line>,
    alt: bool,
    app_cursor: bool,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        let (tx, rx) = channel();
        let config = Config {
            scrolling_history: HISTORY,
            ..Config::default()
        };
        let size = TermSize::new(cols as usize, rows as usize);
        Terminal {
            term: Term::new(config, &size, Listener(tx)),
            parser: Processor::new(),
            term_events: rx,
            filter: Filter::default(),
            ids: HashSet::new(),
            screen: Vec::new(),
            alt: false,
            app_cursor: false,
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
            .map(|y| render(&grid[GridLine(y as i32)], None).text() + "\n")
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
        let block = match block.render() {
            Ok(block) => block,
            Err(e) => return eprintln!("rt: invalid rich block: {e}"),
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
            let line = render(&grid[GridLine(-i)], None);
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
            if let TermEvent::PtyWrite(text) = event {
                update.reply.push_str(&text);
            }
        }

        let mode = *self.term.mode();
        let alt = mode.contains(TermMode::ALT_SCREEN);
        if alt != self.alt {
            self.alt = alt;
            self.screen.clear();
            update.events.push(Event::Alt { on: alt });
        }
        let app_cursor = mode.contains(TermMode::APP_CURSOR);
        if app_cursor != self.app_cursor {
            self.app_cursor = app_cursor;
            update.events.push(Event::Mode { app_cursor });
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
                let line = render(&grid[GridLine(y as i32)], x);
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
}

impl Line {
    fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

fn render(row: &GridRow<Cell>, cursor: Option<usize>) -> Line {
    let cols = row.len();
    let end = (0..cols)
        .rev()
        .find(|&x| Some(x) == cursor || !is_blank(&row[Column(x)]))
        .map_or(0, |x| x + 1);
    let mut runs: Vec<Run> = Vec::new();
    for x in 0..end {
        let cell = &row[Column(x)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let style = style(cell, Some(x) == cursor);
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

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' ' && color(cell.bg).is_none() && !cell.flags.contains(Flags::INVERSE)
}

fn style(cell: &Cell, cursor: bool) -> Style {
    let flags = cell.flags;
    let (mut fg, mut bg) = (color(cell.fg), color(cell.bg));
    if flags.contains(Flags::INVERSE) {
        (fg, bg) = (
            Some(bg.unwrap_or("var(--bg)".into())),
            Some(fg.unwrap_or("var(--fg)".into())),
        );
    }
    if cursor {
        (fg, bg) = (
            Some(bg.unwrap_or("var(--bg)".into())),
            Some("var(--cursor)".into()),
        );
    }
    let class = [
        (flags.contains(Flags::BOLD), "b"),
        (flags.contains(Flags::ITALIC), "i"),
        (flags.intersects(Flags::ALL_UNDERLINES), "u"),
        (flags.contains(Flags::STRIKEOUT), "s"),
        (flags.contains(Flags::DIM), "d"),
        (flags.contains(Flags::HIDDEN), "h"),
        (cursor, "cursor"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| *name)
    .collect::<Vec<_>>()
    .join(" ");
    Style { fg, bg, class }
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
}
