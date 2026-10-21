//! The native view (M6, see Spec-Native-grid in the wiki): the grid is drawn
//! by GTK with Pango straight from the core's state, and rich blocks are
//! child web views placed in the flow.

mod blocks;
mod draw;
mod keys;
mod select;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use raterm::config::{Binding, Config};
use raterm::history::Entry;
use raterm::pty::Error;
use raterm::rich::Rendered;
use raterm::session::Session;
use raterm::term::{Event as TermEvent, Line, Modes, Terminal};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use webkit2gtk::{WebContext, WebView, WebViewExt};

use blocks::block_style;
use draw::{cells, measure};
use keys::encode;
use select::{Pos, Selection, Unit};

enum UserEvent {
    /// New output: lay the flow out again and draw it at the next frame.
    Redraw,
    Title(String),
    Exit,
}

const PAD_X: f64 = 4.0;
const PAD_Y: f64 = 2.0;
/// Rows a block takes until its web view has measured it.
const BLOCK_ROWS: usize = 3;

/// One row of the flow, as laid out for drawing.
enum Visual<'a> {
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

struct View {
    session: Arc<Session>,
    area: gtk::DrawingArea,
    fixed: gtk::Fixed,
    font: pango::FontDescription,
    cell: (f64, f64),
    size: (u16, u16),
    /// Rows scrolled up from the end of the flow.
    scroll: usize,
    /// The history's end at the last layout, to keep the view in place when
    /// output comes while scrolled up.
    seen_next: u64,
    /// Web views of the blocks in view.
    blocks: HashMap<u64, WebView>,
    /// Heights of the blocks measured so far, in pixels; filled by the web
    /// views once loaded, kept when they leave the view.
    heights: Rc<RefCell<HashMap<u64, f64>>>,
    context: Option<WebContext>,
    redraw: Arc<Redraw>,
    /// This view, for the handlers of the web views it makes.
    me: Weak<RefCell<View>>,
    block_style: String,
    selection: Option<Selection>,
    /// The block whose web view was clicked last, which may hold a
    /// selection of its own.
    block_selected: Option<u64>,
    keys: BTreeMap<String, Binding>,
}

/// Asks the event loop for a redraw, once until it is done.
struct Redraw {
    pending: AtomicBool,
    proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl Redraw {
    fn request(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.proxy.lock().unwrap().send_event(UserEvent::Redraw);
        }
    }
}

pub fn run(config: Config, command: &[String], title: String) -> Result<(), Error> {
    let (session, reader) = Session::spawn(&config, command, 80, 24)?;
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(&title)
        .with_decorations(config.window.decorations)
        .build(&event_loop)?;

    let area = gtk::DrawingArea::new();
    area.set_can_focus(true);
    area.add_events(
        gdk::EventMask::SCROLL_MASK
            | gdk::EventMask::SMOOTH_SCROLL_MASK
            | gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK,
    );
    let fixed = gtk::Fixed::new();
    let overlay = gtk::Overlay::new();
    overlay.add(&area);
    overlay.add_overlay(&fixed);
    // Input goes to the grid, except over the web views of blocks.
    overlay.set_overlay_pass_through(&fixed, true);
    let vbox = window.default_vbox().ok_or("no GTK container")?;
    vbox.pack_start(&overlay, true, true, 0);
    overlay.show_all();
    area.grab_focus();

    let font = pango::FontDescription::from_string(&format!(
        "{} {}",
        config.font.family, config.font.size
    ));
    let redraw = Arc::new(Redraw {
        pending: AtomicBool::new(false),
        proxy: Mutex::new(proxy.clone()),
    });
    let modes = Arc::new(Mutex::new(Modes::default()));
    let cell = measure(&area, &font);
    let view = Rc::new(RefCell::new(View {
        session: session.clone(),
        block_style: block_style(&config, cell.0),
        cell,
        area: area.clone(),
        fixed,
        font,
        size: (0, 0),
        scroll: 0,
        seen_next: 0,
        blocks: HashMap::new(),
        heights: Rc::default(),
        context: None,
        redraw: redraw.clone(),
        me: Weak::new(),
        selection: None,
        block_selected: None,
        keys: keys::bindings(&config),
    }));
    view.borrow_mut().me = Rc::downgrade(&view);

    {
        let view = view.clone();
        area.connect_size_allocate(move |_, alloc| view.borrow_mut().resized(alloc));
    }
    {
        let view = view.clone();
        area.connect_draw(move |_, cr| {
            view.borrow().draw(cr);
            glib::Propagation::Stop
        });
    }
    {
        let view = view.clone();
        area.connect_scroll_event(move |_, ev| {
            view.borrow_mut().wheel(ev);
            glib::Propagation::Stop
        });
    }
    {
        let view = view.clone();
        area.connect_button_press_event(move |area, ev| {
            area.grab_focus();
            let (x, y) = ev.position();
            let mut view = view.borrow_mut();
            match ev.button() {
                1 => {
                    let unit = match ev.event_type() {
                        gdk::EventType::DoubleButtonPress => Unit::Word,
                        gdk::EventType::TripleButtonPress => Unit::Line,
                        _ => Unit::Cell,
                    };
                    view.clear_selections(None);
                    if let Some(pos) = view.pos_at(x, y) {
                        view.selection = Some(Selection {
                            anchor: pos,
                            head: pos,
                            unit,
                        });
                        view.area.queue_draw();
                    }
                }
                2 => view.paste(gdk::SELECTION_PRIMARY),
                _ => {}
            }
            glib::Propagation::Stop
        });
    }
    {
        let view = view.clone();
        area.connect_motion_notify_event(move |_, ev| {
            let (x, y) = ev.position();
            let mut view = view.borrow_mut();
            if let Some(pos) = view.pos_at(x, y)
                && let Some(selection) = &mut view.selection
                && selection.head != pos
            {
                selection.head = pos;
                view.area.queue_draw();
            }
            glib::Propagation::Stop
        });
    }
    {
        let view = view.clone();
        area.connect_button_release_event(move |_, ev| {
            if ev.button() == 1 {
                let mut view = view.borrow_mut();
                if view.selection.is_some_and(|s| s.is_empty()) {
                    view.selection = None;
                    view.area.queue_draw();
                }
                view.copy(gdk::SELECTION_PRIMARY);
            }
            glib::Propagation::Stop
        });
    }
    let im = gtk::IMMulticontext::new();
    {
        let session = session.clone();
        let view = view.clone();
        im.connect_commit(move |_, text| {
            view.borrow_mut().scroll_to_end();
            session.input(text.as_bytes());
        });
    }
    {
        let im = im.clone();
        area.connect_realize(move |area| im.set_client_window(area.window().as_ref()));
    }
    {
        let im = im.clone();
        area.connect_focus_in_event(move |_, _| {
            im.focus_in();
            glib::Propagation::Proceed
        });
    }
    {
        let session = session.clone();
        let view = view.clone();
        let modes = modes.clone();
        area.connect_key_press_event(move |_, ev| {
            let binding = view.borrow().keys.get(&keys::name(ev)).cloned();
            match binding {
                Some(Binding::Send { send }) => {
                    view.borrow_mut().scroll_to_end();
                    session.input(send.as_bytes());
                    return glib::Propagation::Stop;
                }
                Some(Binding::Action(action)) if view.borrow_mut().run_action(&action) => {
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
            if let Some(data) = encode(ev, &modes.lock().unwrap()) {
                view.borrow_mut().scroll_to_end();
                session.input(data.as_bytes());
                return glib::Propagation::Stop;
            }
            if im.filter_keypress(ev) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    let pump = session.clone();
    let title_proxy = proxy.clone();
    thread::spawn(move || {
        pump.pump(reader, |events| {
            for event in &events {
                match event {
                    TermEvent::Title { title } => {
                        let _ = title_proxy.send_event(UserEvent::Title(title.clone()));
                    }
                    TermEvent::Mode(m) => *modes.lock().unwrap() = *m,
                    _ => {}
                }
            }
            redraw.request();
        });
        let _ = title_proxy.send_event(UserEvent::Exit);
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Redraw) => {
                let mut v = view.borrow_mut();
                v.redraw.pending.store(false, Ordering::Release);
                v.layout();
            }
            Event::UserEvent(UserEvent::Title(set)) => {
                window.set_title(if set.is_empty() { &title } else { &set })
            }
            Event::UserEvent(UserEvent::Exit)
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    })
}

impl View {
    fn resized(&mut self, alloc: &gtk::Allocation) {
        let (cw, ch) = self.cell;
        let cols = ((alloc.width() as f64 - 2.0 * PAD_X) / cw).floor().max(1.0) as u16;
        let rows = ((alloc.height() as f64 - 2.0 * PAD_Y) / ch)
            .floor()
            .max(1.0) as u16;
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            let redraw = self.redraw.clone();
            self.session.resize(cols, rows, |_| redraw.request());
        }
    }

    fn wheel(&mut self, ev: &gdk::EventScroll) {
        let dy = match ev.direction() {
            gdk::ScrollDirection::Up => -1.0,
            gdk::ScrollDirection::Down => 1.0,
            _ => ev.delta().1,
        };
        self.scroll = (self.scroll as f64 - 3.0 * dy).max(0.0) as usize;
        self.layout();
    }

    /// The position in the flow at a point of the view: the nearest boundary
    /// between cells, or the start of a block.
    fn pos_at(&self, x: f64, y: f64) -> Option<Pos> {
        let (cw, ch) = self.cell;
        self.session.read(|term| {
            let visuals = self.visuals(term);
            let row =
                (((y - PAD_Y) / ch).floor().max(0.0) as usize).min(visuals.len().checked_sub(1)?);
            Some(match visuals[row] {
                Visual::Text { start, at, .. } => {
                    let col = ((x - PAD_X) / cw).round().clamp(0.0, self.size.0 as f64) as usize;
                    Pos {
                        at,
                        cell: start + col,
                    }
                }
                Visual::Block { entry, .. } => Pos { at: entry, cell: 0 },
            })
        })
    }

    /// Runs a shortcut's action; false lets the key reach the program.
    fn run_action(&mut self, action: &str) -> bool {
        match action {
            "copy" => self.copy(gdk::SELECTION_CLIPBOARD),
            "paste" => self.paste(gdk::SELECTION_CLIPBOARD),
            "paste_selection" => self.paste(gdk::SELECTION_PRIMARY),
            "scroll_page_up" | "scroll_page_down" => {
                if self.session.read(|t| t.alt_screen()) {
                    return false;
                }
                let page = self.size.1 as usize * 9 / 10;
                self.scroll = if action == "scroll_page_up" {
                    self.scroll + page
                } else {
                    self.scroll.saturating_sub(page)
                };
                self.layout();
            }
            "new_window" => new_window(self.session.cwd()),
            // Not in the native view yet.
            "search" | "zoom_in" | "zoom_out" | "zoom_reset" => {}
            _ => return false,
        }
        true
    }

    /// Copies the selection to the clipboard or the primary selection; that
    /// of a block, the web view copies itself.
    fn copy(&self, which: gdk::Atom) {
        let Some(selection) = self.selection else {
            if which == gdk::SELECTION_CLIPBOARD
                && let Some(view) = self.block_selected.and_then(|e| self.blocks.get(&e))
            {
                view.execute_editing_command("Copy");
            }
            return;
        };
        let text = self.session.read(|term| selection.text(term));
        gtk::Clipboard::get(&which).set_text(&text);
    }

    fn paste(&self, which: gdk::Atom) {
        let session = self.session.clone();
        gtk::Clipboard::get(&which).request_text(move |_, text| {
            if let Some(text) = text {
                session.paste(text);
            }
        });
    }

    fn scroll_to_end(&mut self) {
        if self.scroll != 0 {
            self.scroll = 0;
            self.layout();
        }
    }

    /// Places the web views of the blocks in view, then queues a draw.
    fn layout(&mut self) {
        let session = self.session.clone();
        let mut shown: Vec<(u64, Rendered, f64, usize)> = Vec::new();
        session.read(|term| {
            // Keep the view where it is when output comes while scrolled up.
            let next = term.history.next();
            if self.scroll > 0 {
                self.scroll += (next - self.seen_next) as usize;
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
        let width = (self.size.0 as f64 * self.cell.0) as i32;
        for (entry, block, y, rows) in &shown {
            if !self.blocks.contains_key(entry) {
                let view = self.block_view(*entry, block);
                self.fixed.put(&view, PAD_X as i32, *y as i32);
                view.show();
                self.blocks.insert(*entry, view);
            }
            let view = &self.blocks[entry];
            view.set_size_request(width, (*rows as f64 * self.cell.1) as i32);
            self.fixed.move_(view, PAD_X as i32, *y as i32);
        }
        // Blocks out of view go; they are made again when they come back.
        let visible: Vec<u64> = shown.iter().map(|(e, ..)| *e).collect();
        self.blocks.retain(|entry, view| {
            let keep = visible.contains(entry);
            if !keep {
                unsafe { view.destroy() };
            }
            keep
        });
        self.area.queue_draw();
    }

    /// The rows in view, top to bottom; the flow starts at the top of the view
    /// until it fills it.
    fn visuals<'a>(&self, term: &'a Terminal) -> Vec<Visual<'a>> {
        let (cols, rows) = (self.size.0.max(1) as usize, self.size.1 as usize);
        let screen = term.screen();
        // Empty rows below the last used one are not shown, as in the page,
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
                        let rows = self
                            .heights
                            .borrow()
                            .get(&n)
                            .map_or(BLOCK_ROWS, |h| (h / self.cell.1).ceil().max(1.0) as usize);
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

/// Opens another native window, in the same directory as this one's shell.
fn new_window(cwd: Option<std::path::PathBuf>) {
    let mut command = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe),
        Err(e) => return eprintln!("raterm: cannot find own executable: {e}"),
    };
    command.arg("--native");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    match command.spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("raterm: cannot open a new window: {e}"),
    }
}
