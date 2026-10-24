//! What the view decides, whatever draws it (see Spec-View and Spec-macOS):
//! the flow of lines and blocks in view, scrolling, positions under the
//! pointer, mouse reporting, links, search and shortcuts. Backends hold a
//! `Flow`, draw what it lays out and give it their input.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use raterm::config::{Binding, Config};
use raterm::history::Entry;
use raterm::rich::{Kind, Rendered};
use raterm::session::Session;
use raterm::term::{Line, Modes, Terminal};

use super::keys::{self, Mods};
use super::runs::cells;
use super::search::Search;
use super::select::{Pos, Selection};

pub(super) const PAD_X: f64 = 4.0;
pub(super) const PAD_Y: f64 = 2.0;
/// Rows a block takes until its web view has measured it.
const BLOCK_ROWS: usize = 3;

/// One row of the flow, as laid out for drawing.
pub(super) enum Visual<'a> {
    /// Cells `start..` of a line, up to the width of the view; `at` is the
    /// line's position in the flow (see `select::Pos`).
    Text {
        line: &'a Line,
        start: usize,
        at: u64,
    },
    /// Row `row` of a block, `rows` tall.
    Block { entry: u64, row: usize, rows: usize },
}

/// A block in view, where its web view goes.
pub(super) struct Spot {
    pub(super) entry: u64,
    pub(super) block: Rendered,
    pub(super) x: i32,
    /// The top of the block, maybe above the view when it is cut by its top.
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

/// What the backend does for a shortcut's action (see `Flow::action`).
pub(super) enum Action {
    /// The flow scrolled: lay it out again.
    Layout,
    Copy(Clipboard),
    Paste(Clipboard),
    NewWindow,
    OpenSearch,
    /// The new step of the zoom.
    Zoom(i32),
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Clipboard {
    Clipboard,
    /// The primary selection, where there is one (Linux).
    Primary,
}

/// A step of the search in a block.
pub(super) enum Find {
    Start { query: String, backwards: bool },
    Next,
    Previous,
    Finish,
}

/// What a step of the search found (see `Flow::search_step`).
pub(super) struct Found {
    /// Whether there is a match, or nothing to look for.
    pub(super) any: bool,
    /// The searches to run in the web views of blocks, once laid out.
    pub(super) finds: Vec<(u64, Find)>,
}

pub(super) struct Flow {
    pub(super) session: Arc<Session>,
    pub(super) config: Config,
    pub(super) keys: BTreeMap<String, Binding>,
    /// Modes of the program, as the core last sent them.
    pub(super) modes: Arc<Mutex<Modes>>,
    /// The size of a cell in pixels, and of the grid in cells.
    pub(super) cell: (f64, f64),
    pub(super) size: (u16, u16),
    /// Steps of the zoom, each 10% bigger or smaller.
    pub(super) zoom: i32,
    /// Rows scrolled up from the end of the flow.
    pub(super) scroll: usize,
    /// The history's end at the last layout, to keep the view in place when
    /// output comes while scrolled up.
    seen_next: u64,
    /// The scroll at the last layout, to see it change.
    laid_scroll: usize,
    /// Heights of the blocks measured so far, in pixels; filled by the web
    /// views once loaded, kept when they leave the view.
    pub(super) heights: Rc<RefCell<HashMap<u64, f64>>>,
    /// The natural width of the widest table of each block measured, for
    /// `[blocks] padding`.
    pub(super) table_widths: Rc<RefCell<HashMap<u64, f64>>>,
    pub(super) selection: Option<Selection>,
    /// The block whose web view was clicked last, which may hold a
    /// selection of its own.
    pub(super) block_selected: Option<u64>,
    /// Open while the search bar is.
    pub(super) search: Option<Search>,
    /// When the view last scrolled: a thin indicator shows where it is in the
    /// flow, then fades, as GTK's overlay scrollbars.
    pub(super) scrolled_at: Rc<Cell<Option<Instant>>>,
    /// Whether the window has the focus, and the blinking cursor is shown.
    pub(super) focused: Cell<bool>,
    pub(super) blink_on: Cell<bool>,
    /// The button held down and the last cell reported, while the program
    /// gets the mouse.
    pub(super) pressed: Option<u8>,
    pub(super) last_cell: Option<(usize, usize)>,
    /// The link under a Ctrl+click, opened on release unless a block was
    /// dragged.
    pub(super) link: Option<String>,
}

impl Flow {
    pub(super) fn new(
        session: Arc<Session>,
        config: Config,
        modes: Arc<Mutex<Modes>>,
        cell: (f64, f64),
    ) -> Flow {
        Flow {
            session,
            keys: keys::bindings(&config),
            config,
            modes,
            cell,
            size: (0, 0),
            zoom: 0,
            scroll: 0,
            seen_next: 0,
            laid_scroll: 0,
            heights: Rc::default(),
            table_widths: Rc::default(),
            selection: None,
            block_selected: None,
            search: None,
            scrolled_at: Rc::default(),
            focused: Cell::new(true),
            blink_on: Cell::new(true),
            pressed: None,
            last_cell: None,
            link: None,
        }
    }

    /// The factor of the zoom, on the sizes of the configuration.
    pub(super) fn scale(&self) -> f64 {
        1.1f64.powi(self.zoom)
    }

    /// Sizes the grid to a view of `width × height` pixels; the pty gets the
    /// new size, and `redraw` is called once it has.
    pub(super) fn resized(&mut self, width: f64, height: f64, redraw: impl Fn() + Send + 'static) {
        let (cw, ch) = self.cell;
        let cols = ((width - 2.0 * PAD_X) / cw).floor().max(1.0) as u16;
        let rows = ((height - 2.0 * PAD_Y) / ch).floor().max(1.0) as u16;
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.session.resize(cols, rows, self.cell, |_| redraw());
        }
    }

    /// The wheel, `dy` rows down (negative: up), at a point of the view;
    /// true when the flow scrolled and needs to be laid out again.
    pub(super) fn wheel(&mut self, dy: f64, mods: Mods, x: f64, y: f64) -> bool {
        if dy == 0.0 {
            return false;
        }
        if let Some(cell) = self.reported_cell(mods, x, y) {
            self.send_mouse(if dy < 0.0 { 64 } else { 65 }, mods, cell, false);
            return false;
        }
        let modes = *self.modes.lock().unwrap();
        if self.session.read(|t| t.alt_screen()) && modes.alt_scroll && !mods.shift {
            // Full-screen programs without mouse reporting (less, man)
            // scroll with arrow keys.
            let arrow = if modes.app_cursor { "\x1bO" } else { "\x1b[" };
            let key = format!("{arrow}{}", if dy < 0.0 { "A" } else { "B" });
            self.session.input(key.repeat(3).as_bytes());
            return false;
        }
        self.scroll = (self.scroll as f64 - 3.0 * dy).max(0.0) as usize;
        true
    }

    /// The position in the flow at a point of the view: the nearest boundary
    /// between cells (the cell under the point for a block selection, which
    /// includes it), or the start of a rich block.
    pub(super) fn pos_at(&self, x: f64, y: f64, block: bool) -> Option<Pos> {
        let (cw, ch) = self.cell;
        self.session.read(|term| {
            let visuals = self.visuals(term);
            let row =
                (((y - PAD_Y) / ch).floor().max(0.0) as usize).min(visuals.len().checked_sub(1)?);
            Some(match visuals[row] {
                Visual::Text { start, at, .. } => {
                    let cols = self.size.0 as f64;
                    let col = match block {
                        true => ((x - PAD_X) / cw).floor().clamp(0.0, cols - 1.0),
                        false => ((x - PAD_X) / cw).round().clamp(0.0, cols),
                    } as usize;
                    Pos {
                        at,
                        cell: start + col,
                    }
                }
                Visual::Block { entry, .. } => Pos { at: entry, cell: 0 },
            })
        })
    }

    /// The cell of the screen at a point of the view, if any.
    pub(super) fn screen_cell(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let (cw, ch) = self.cell;
        if x < PAD_X || y < PAD_Y {
            return None;
        }
        let col = ((x - PAD_X) / cw) as usize;
        let row = ((y - PAD_Y) / ch) as usize;
        self.session.read(|term| {
            let next = term.history.next();
            match self.visuals(term).get(row)? {
                Visual::Text { at, .. } if *at >= next && col < self.size.0 as usize => {
                    Some((col, (*at - next) as usize))
                }
                _ => None,
            }
        })
    }

    /// The cell the program gets a mouse event at, when it asks for them and
    /// Shift does not keep the mouse for selecting.
    pub(super) fn reported_cell(&self, mods: Mods, x: f64, y: f64) -> Option<(usize, usize)> {
        if self.modes.lock().unwrap().mouse == 0 || mods.shift {
            return None;
        }
        self.screen_cell(x, y)
    }

    /// Reports a motion, when the program asks for drags (2) or any motion
    /// (3); true if it was reported.
    pub(super) fn report_motion(&mut self, mods: Mods, x: f64, y: f64) -> bool {
        let mouse = self.modes.lock().unwrap().mouse;
        if mouse < 2 || (mouse == 2 && self.pressed.is_none()) {
            return false;
        }
        let Some(cell) = self.reported_cell(mods, x, y) else {
            return false;
        };
        if self.last_cell != Some(cell) {
            self.last_cell = Some(cell);
            self.send_mouse(32 + self.pressed.unwrap_or(3), mods, cell, false);
        }
        true
    }

    pub(super) fn send_mouse(&self, button: u8, mods: Mods, cell: (usize, usize), release: bool) {
        let sgr = self.modes.lock().unwrap().sgr_mouse;
        if let Some(data) = keys::mouse(button, mods, cell, release, sgr) {
            self.session.input(&data);
        }
    }

    /// The link of the run at a point of the view (OSC 8 or a URL in the text).
    pub(super) fn link_at(&self, x: f64, y: f64) -> Option<String> {
        let (cw, ch) = self.cell;
        let row = ((y - PAD_Y) / ch).floor();
        let col = ((x - PAD_X) / cw).floor();
        if row < 0.0 || col < 0.0 {
            return None;
        }
        self.session.read(|term| {
            let visuals = self.visuals(term);
            let Visual::Text { line, start, .. } = visuals.get(row as usize)? else {
                return None;
            };
            let cell = start + col as usize;
            let mut x0 = 0;
            for run in &line.runs {
                let n = cells(run);
                if cell < x0 + n {
                    return run.style.link.clone();
                }
                x0 += n;
            }
            None
        })
    }

    /// Rows a block takes: its measured height, or a guess until then.
    pub(super) fn block_rows(&self, n: u64) -> usize {
        self.heights
            .borrow()
            .get(&n)
            .map_or(BLOCK_ROWS, |h| (h / self.cell.1).ceil().max(1.0) as usize)
    }

    /// Rows of the flow below line `at`: how far to scroll up to see it.
    fn rows_below(&self, term: &Terminal, at: u64) -> usize {
        let cols = self.size.0.max(1) as usize;
        let screen = term.screen();
        let used = screen
            .iter()
            .rposition(|l| !l.runs.is_empty())
            .map_or(1, |y| y + 1);
        let history = &term.history;
        if at >= history.next() {
            return used.saturating_sub((at - history.next()) as usize + 1);
        }
        let mut rows = used;
        for n in at + 1..history.next() {
            rows += match history.get(n) {
                Some(Entry::Line(line)) => line
                    .runs
                    .iter()
                    .map(cells)
                    .sum::<usize>()
                    .max(1)
                    .div_ceil(cols),
                Some(Entry::Block(_)) => self.block_rows(n),
                None => 0,
            };
        }
        rows
    }

    /// Scrolls the flow to put height `top` of a block in the middle; lay it
    /// out again then.
    pub(super) fn reveal_in_block(&mut self, entry: u64, top: f64) {
        let rows = self.block_rows(entry);
        let row = ((top / self.cell.1) as usize).min(rows.saturating_sub(1));
        let below = self.session.read(|t| self.rows_below(t, entry)) + rows - 1 - row;
        self.scroll = below.saturating_sub(self.size.1 as usize / 2);
    }

    /// Goes to the next match of `query`, older or newer, or from the end for
    /// a new query; the flow scrolls to put it in the middle. `in_view` tells
    /// whether a block has a web view: in a block in view, the flow scrolls
    /// once WebKit tells where the match is (`reveal_in_block`). Lay the flow
    /// out, then run the finds.
    pub(super) fn search_step(
        &mut self,
        query: &str,
        restart: bool,
        backwards: bool,
        in_view: impl Fn(u64) -> bool,
    ) -> Option<Found> {
        let search = self.search.as_mut()?;
        if restart {
            search.set_query(query);
        }
        let from = search.current.map(|(at, start, _)| (at, start));
        let found = self.session.read(|t| t.search(query, from, backwards));
        let any = search.found(found) || query.is_empty();
        let previous = search.block;
        let current = search.current.map(|(at, ..)| at);
        let block = current.filter(|&at| {
            self.session
                .read(|t| matches!(t.history.get(at), Some(Entry::Block(_))))
        });
        search.block = block;
        // Scrolling to a block in view first showed a frame elsewhere. A
        // block out of view needs its web view first.
        if let Some(at) = current
            && !block.is_some_and(&in_view)
        {
            let rows = self.size.1 as usize;
            let below = self.session.read(|t| self.rows_below(t, at));
            self.scroll = below.saturating_sub(rows / 2);
        }
        // WebKit moves from match to match along with the core while the
        // current match stays in the same block.
        let mut finds = Vec::new();
        match (previous, block) {
            (Some(p), Some(b)) if p == b && !restart => finds.push((
                b,
                if backwards {
                    Find::Previous
                } else {
                    Find::Next
                },
            )),
            _ => {
                if let Some(p) = previous {
                    finds.push((p, Find::Finish));
                }
                if let Some(b) = block {
                    let query = query.to_owned();
                    finds.push((b, Find::Start { query, backwards }));
                }
            }
        }
        Some(Found { any, finds })
    }

    /// Runs a shortcut's action, or tells what the backend has to do; `None`
    /// lets the key reach the program.
    pub(super) fn action(&mut self, action: &str) -> Option<Action> {
        Some(match action {
            "copy" => Action::Copy(Clipboard::Clipboard),
            "paste" => Action::Paste(Clipboard::Clipboard),
            "paste_selection" => Action::Paste(Clipboard::Primary),
            "scroll_page_up" | "scroll_page_down" => {
                if self.session.read(|t| t.alt_screen()) {
                    return None;
                }
                let page = self.size.1 as usize * 9 / 10;
                self.scroll = if action == "scroll_page_up" {
                    self.scroll + page
                } else {
                    self.scroll.saturating_sub(page)
                };
                Action::Layout
            }
            "new_window" => Action::NewWindow,
            "search" => Action::OpenSearch,
            "zoom_in" => Action::Zoom(self.zoom + 1),
            "zoom_out" => Action::Zoom(self.zoom - 1),
            "zoom_reset" => Action::Zoom(0),
            _ => return None,
        })
    }

    /// The text of the selection of the grid, to copy.
    pub(super) fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        Some(self.session.read(|term| selection.text(term)))
    }

    /// Lays the flow out: the blocks in view and where they go; true in
    /// the second place when the user scrolled since, for the indicator.
    pub(super) fn layout(&mut self) -> (Vec<Spot>, bool) {
        let session = self.session.clone();
        let mut shown: Vec<(u64, Rendered, f64, usize)> = Vec::new();
        session.read(|term| {
            // Keep the view where it is when output comes while scrolled up.
            let next = term.history.next();
            if self.scroll > 0 {
                let rows = (next - self.seen_next) as usize;
                self.scroll += rows;
                // Output does not show the scroll indicator: only the user's
                // scrolling does.
                self.laid_scroll += rows;
            }
            // Positions of the screen are its rows: they move when output
            // scrolls it, the selection of history entries stays.
            if next != self.seen_next
                && let Some(selection) = self.selection
                && selection.anchor.at.max(selection.head.at) >= self.seen_next
            {
                self.selection = None;
            }
            self.seen_next = next;
            let mut y = PAD_Y;
            for visual in &self.visuals(term) {
                // The first row in view of a block, maybe not its first row:
                // a block cut by the top of the view starts above it.
                if let Visual::Block { entry, row, rows } = *visual
                    && !shown.iter().any(|(e, ..)| *e == entry)
                    && let Some(Entry::Block(block)) = term.history.get(entry)
                {
                    shown.push((entry, block.clone(), y - row as f64 * self.cell.1, rows));
                }
                y += self.cell.1;
            }
        });
        let spots = shown
            .into_iter()
            .map(|(entry, block, y, rows)| {
                let (x, width) = self.block_place(entry, &block);
                Spot {
                    entry,
                    x,
                    y: y as i32,
                    width,
                    height: (rows as f64 * self.cell.1) as i32,
                    block,
                }
            })
            .collect();
        let scrolled = self.scroll != self.laid_scroll;
        self.laid_scroll = self.scroll;
        (spots, scrolled)
    }

    /// Where a block goes across the view, and how wide: the whole width,
    /// or with `[blocks] padding` a Markdown block as wide as its column of
    /// text or its widest table, plus the padding, centered.
    fn block_place(&self, entry: u64, block: &Rendered) -> (i32, i32) {
        let cw = self.cell.0;
        let full = self.size.0 as f64 * cw;
        let columns = self.config.markdown.max_width as f64;
        let width = match self.config.blocks.padding {
            Some(padding) if block.kind == Kind::Md && columns > 0.0 => {
                let table = self.table_widths.borrow().get(&entry).copied();
                let content = (columns * cw).max(table.unwrap_or(0.0));
                (content + 2.0 * padding as f64 * cw).min(full)
            }
            _ => full,
        };
        ((PAD_X + (full - width) / 2.0) as i32, width as i32)
    }

    /// The rows in view, top to bottom; the flow starts at the top of the view
    /// until it fills it.
    pub(super) fn visuals<'a>(&self, term: &'a Terminal) -> Vec<Visual<'a>> {
        let (cols, rows) = (self.size.0.max(1) as usize, self.size.1 as usize);
        let screen = term.screen();
        // Empty rows below the last used one are not shown,
        // so that output stays right above the prompt.
        let used = if term.alt_screen() {
            screen.len()
        } else {
            screen
                .iter()
                .rposition(|l| !l.runs.is_empty())
                .map_or(1, |y| y + 1)
        };
        let mut out: Vec<Visual> = Vec::new();
        let want = rows + self.scroll;
        let next = term.history.next();
        for (y, line) in screen[..used].iter().enumerate().rev() {
            out.push(Visual::Text {
                line,
                start: 0,
                at: next + y as u64,
            });
        }
        if !term.alt_screen() {
            let history = &term.history;
            let mut n = history.next();
            while out.len() < want && n > history.first() {
                n -= 1;
                match history.get(n) {
                    Some(Entry::Line(line)) => {
                        let len: usize = line.runs.iter().map(cells).sum();
                        let wraps = len.max(1).div_ceil(cols);
                        for i in (0..wraps).rev() {
                            out.push(Visual::Text {
                                line,
                                start: i * cols,
                                at: n,
                            });
                        }
                    }
                    Some(Entry::Block(_)) => {
                        let rows = self.block_rows(n);
                        for row in (0..rows).rev() {
                            out.push(Visual::Block {
                                entry: n,
                                row,
                                rows,
                            });
                        }
                    }
                    None => {}
                }
            }
        }
        let scroll = self.scroll.min(out.len().saturating_sub(rows));
        let end = (scroll + rows).min(out.len());
        let mut shown: Vec<Visual> = out.drain(scroll..end).collect();
        shown.reverse();
        shown
    }
}
