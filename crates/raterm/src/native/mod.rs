//! The native view (M6, see Spec-Native-grid in the wiki): the grid is drawn
//! by GTK with Pango straight from the core's state, and rich blocks are
//! child web views placed in the flow.

mod blocks;
mod draw;
mod keys;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use raterm::config::Config;
use raterm::history::Entry;
use raterm::pty::Error;
use raterm::rich::Rendered;
use raterm::session::Session;
use raterm::term::{Event as TermEvent, Line, Modes, Terminal};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use webkit2gtk::{WebContext, WebView};

use blocks::block_style;
use draw::{cells, measure};
use keys::encode;

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
pub(super) enum Visual<'a> {
    /// Cells `start..` of a line, up to the width of the view.
    Text { line: &'a Line, start: usize },
    /// Row `row` of a block, `rows` tall.
    Block { entry: u64, row: usize, rows: usize },
}

pub(super) struct View {
    pub(super) session: Arc<Session>,
    pub(super) area: gtk::DrawingArea,
    pub(super) fixed: gtk::Fixed,
    pub(super) font: pango::FontDescription,
    pub(super) cell: (f64, f64),
    pub(super) size: (u16, u16),
    /// Rows scrolled up from the end of the flow.
    pub(super) scroll: usize,
    /// The history's end at the last layout, to keep the view in place when
    /// output comes while scrolled up.
    pub(super) seen_next: u64,
    /// Web views of the blocks in view.
    pub(super) blocks: HashMap<u64, WebView>,
    /// Heights of the blocks measured so far, in pixels; filled by the web
    /// views once loaded, kept when they leave the view.
    pub(super) heights: Rc<RefCell<HashMap<u64, f64>>>,
    pub(super) context: Option<WebContext>,
    pub(super) redraw: Arc<Redraw>,
    /// This view, for the handlers of the web views it makes.
    pub(super) me: Weak<RefCell<View>>,
    pub(super) block_style: String,
}

/// Asks the event loop for a redraw, once until it is done.
pub(super) struct Redraw {
    pending: AtomicBool,
    proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl Redraw {
    pub(super) fn request(&self) {
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
    area.add_events(gdk::EventMask::SCROLL_MASK | gdk::EventMask::SMOOTH_SCROLL_MASK);
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
            let mods = ev.state();
            if mods.contains(gdk::ModifierType::SHIFT_MASK)
                && matches!(
                    ev.keyval(),
                    gdk::keys::constants::Page_Up | gdk::keys::constants::Page_Down
                )
            {
                let mut view = view.borrow_mut();
                let page = view.size.1 as usize * 9 / 10;
                view.scroll = if ev.keyval() == gdk::keys::constants::Page_Up {
                    view.scroll + page
                } else {
                    view.scroll.saturating_sub(page)
                };
                view.layout();
                return glib::Propagation::Stop;
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

    pub(super) fn wheel(&mut self, ev: &gdk::EventScroll) {
        let dy = match ev.direction() {
            gdk::ScrollDirection::Up => -1.0,
            gdk::ScrollDirection::Down => 1.0,
            _ => ev.delta().1,
        };
        self.scroll = (self.scroll as f64 - 3.0 * dy).max(0.0) as usize;
        self.layout();
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
        for line in screen[..used].iter().rev() {
            out.push(Visual::Text { line, start: 0 });
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
