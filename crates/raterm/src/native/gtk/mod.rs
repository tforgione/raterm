//! The view on Linux (see Spec-View in the wiki): the grid is drawn
//! by GTK with Pango straight from the core's state, and rich blocks are
//! child web views placed in the flow, which `Flow` lays out.

mod blocks;
mod draw;
mod input;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use gtk::prelude::*;
use gtk::{cairo, gdk, gdk_pixbuf, glib, pango};
use raterm::config::{Binding, Config};
use raterm::pty::Error;
use raterm::session::Session;
use raterm::term::Modes;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use webkit2gtk::WebView;

use crate::native::blocks::{BlockView, block_style, css_fonts};
use crate::native::events::{self, Redraw, UserEvent};
use crate::native::flow::{Action, Find, Flow, PAD_Y};
use crate::native::keys::{self, encode};
use crate::native::select::{Selection, Unit};
use crate::native::system::{Clipboard, Which, new_window, open_url, run_command};
use crate::native::theme;
use draw::measure;
use input::mods;

struct View {
    flow: Flow,
    session: Arc<Session>,
    area: gtk::DrawingArea,
    fixed: gtk::Fixed,
    font: pango::FontDescription,
    /// The baseline of `font` from the top of a cell, which glyphs of
    /// fallback fonts are drawn on.
    baseline: f64,
    /// The chars `font` has glyphs for, drawn a row at a time.
    coverage: Option<pango::Coverage>,
    /// Web views of the blocks in view.
    blocks: HashMap<u64, WebView>,
    /// Never shown: the web views of blocks are related to it, so that they
    /// share its web process instead of starting one each.
    root: Option<WebView>,
    redraw: Arc<Redraw>,
    /// This view, for the handlers of the web views it makes.
    me: Weak<RefCell<View>>,
    block_style: String,
    search_entry: gtk::Entry,
    overlay: gtk::Overlay,
    /// The images of the rows of the view, kept from frame to frame, and
    /// how many rows the last frame had.
    rows: RefCell<Vec<Option<draw::Row>>>,
    drawn: Cell<usize>,
    /// `[background] image`, and the background composed from it for the
    /// size of the view (see draw.rs).
    image: Option<gdk_pixbuf::Pixbuf>,
    backdrop: RefCell<Option<cairo::ImageSurface>>,
    /// Surfaces of the images cells show, and how many times images were
    /// removed when the rows were drawn.
    images: RefCell<draw::Surfaces>,
    removed: Cell<u64>,
    /// The image shown full window, made on its first use.
    viewer: Option<WebView>,
    media: raterm::media::Server,
}

pub fn run(config: Config, command: &[String], title: String) -> Result<(), Error> {
    // First: everything that draws reads it.
    theme::init_theme(&config.colors);
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(&title)
        .with_decorations(config.window.decorations)
        .with_transparent(config.window.opacity() < 1.0)
        .build(&event_loop)?;

    let area = gtk::DrawingArea::new();
    area.set_can_focus(true);
    area.add_events(
        gdk::EventMask::SCROLL_MASK
            | gdk::EventMask::SMOOTH_SCROLL_MASK
            | gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK,
    );
    let fixed = gtk::Fixed::new();
    let overlay = gtk::Overlay::new();
    overlay.add(&area);
    overlay.add_overlay(&fixed);
    // Input goes to the grid, except over the web views of blocks.
    overlay.set_overlay_pass_through(&fixed, true);
    let search_entry = search_bar(&config);
    overlay.add_overlay(&search_entry);
    let vbox = window.default_vbox().ok_or("no GTK container")?;
    vbox.pack_start(&overlay, true, true, 0);
    overlay.show_all();
    area.grab_focus();

    let font = pango::FontDescription::from_string(&format!(
        "{} {}",
        config.font.family, config.font.size
    ));
    let redraw = Redraw::new(proxy.clone());
    let modes = Arc::new(Mutex::new(Modes::default()));
    let cell = measure(&area, &font);
    // The program starts with the cell's size in pixels, which it may ask
    // for (`CSI 16 t`) before the window is first allocated.
    let (session, output) = Session::spawn(&config, command, 80, 24, cell)?;
    // Local videos and audio are streamed to the web views of blocks.
    let media = raterm::media::serve(session.media())?;
    let baseline = draw::baseline(&area, &font);
    let view = Rc::new(RefCell::new(View {
        flow: Flow::new(session.clone(), config.clone(), modes.clone(), cell),
        session: session.clone(),
        block_style: block_style(&config, cell.0, 1.0),
        baseline,
        area: area.clone(),
        fixed,
        coverage: draw::coverage(&area, &font),
        font,
        blocks: HashMap::new(),
        root: None,
        redraw: redraw.clone(),
        me: Weak::new(),
        search_entry: search_entry.clone(),
        overlay: overlay.clone(),
        rows: RefCell::default(),
        drawn: Cell::default(),
        image: draw::load_image(&config.background),
        backdrop: RefCell::default(),
        images: RefCell::default(),
        removed: Cell::default(),
        viewer: None,
        media,
    }));
    view.borrow_mut().me = Rc::downgrade(&view);

    {
        let view = view.clone();
        area.connect_size_allocate(move |_, alloc| view.borrow_mut().resized(alloc));
    }
    #[cfg(feature = "trace")]
    {
        // The frame clock exists once the window is shown; after-paint is
        // when GTK hands the frame to the compositor.
        let hooked = Cell::new(false);
        area.connect_draw(move |area, _| {
            if !hooked.replace(true)
                && let Some(clock) = area.frame_clock()
            {
                clock.connect_before_paint(|_| raterm::trace!("before-paint"));
                clock.connect_after_paint(|_| raterm::trace!("after-paint"));
            }
            glib::Propagation::Proceed
        });
    }
    {
        let view = view.clone();
        area.connect_draw(move |_, cr| {
            raterm::trace!("draw");
            view.borrow().draw(cr);
            raterm::trace!("drawn");
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
            let mods = mods(ev.state());
            let link = (ev.button() == 1 && mods.ctrl)
                .then(|| view.flow.link_at(x, y))
                .flatten();
            if let Some(cell) = view.flow.reported_cell(mods, x, y)
                && (1..=3).contains(&ev.button())
            {
                // No selection to tell a Ctrl+click on a link apart from.
                if let Some(url) = link {
                    open_url(&url);
                    return glib::Propagation::Stop;
                }
                // The program gets the click: no selection.
                if ev.event_type() == gdk::EventType::ButtonPress {
                    let button = ev.button() as u8 - 1;
                    view.flow.pressed = Some(button);
                    view.flow.last_cell = Some(cell);
                    view.flow.send_mouse(button, mods, cell, false);
                }
                return glib::Propagation::Stop;
            }
            match ev.button() {
                1 => {
                    let unit = match ev.event_type() {
                        gdk::EventType::DoubleButtonPress => Unit::Word,
                        gdk::EventType::TripleButtonPress => Unit::Line,
                        _ => Unit::Cell,
                    };
                    view.clear_selections(None);
                    let block =
                        (mods.ctrl && unit == Unit::Cell).then_some(view.flow.size.0 as usize);
                    view.flow.link = link.filter(|_| unit == Unit::Cell);
                    if let Some(pos) = view.flow.pos_at(x, y, block.is_some()) {
                        view.flow.selection = Some(Selection {
                            anchor: pos,
                            head: pos,
                            unit,
                            block,
                        });
                        view.area.queue_draw();
                    }
                }
                2 => view.paste(Which::Primary),
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
            if view.flow.report_motion(mods(ev.state()), x, y) {
                return glib::Propagation::Stop;
            }
            if !ev.state().contains(gdk::ModifierType::BUTTON1_MASK) {
                return glib::Propagation::Proceed;
            }
            let block = view.flow.selection.is_some_and(|s| s.block.is_some());
            if let Some(pos) = view.flow.pos_at(x, y, block)
                && let Some(selection) = &mut view.flow.selection
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
            let mut view = view.borrow_mut();
            if let Some(button) = view.flow.pressed.take() {
                let (x, y) = ev.position();
                if let Some(cell) = view.flow.screen_cell(x, y).or(view.flow.last_cell) {
                    view.flow.send_mouse(button, mods(ev.state()), cell, true);
                }
                return glib::Propagation::Stop;
            }
            if ev.button() == 1 {
                let link = view.flow.link.take();
                if view.flow.selection.is_some_and(|s| s.is_empty()) {
                    view.flow.selection = None;
                    view.area.queue_draw();
                    if let Some(url) = link {
                        open_url(&url);
                    }
                }
                view.copy(Which::Primary);
            }
            glib::Propagation::Stop
        });
    }
    {
        let view = view.clone();
        search_entry.connect_changed(move |_| view.borrow_mut().search_step(true, true));
    }
    {
        let view = view.clone();
        search_entry.connect_key_press_event(move |_, ev| {
            use gdk::keys::constants as k;
            let shift = ev.state().contains(gdk::ModifierType::SHIFT_MASK);
            match ev.keyval() {
                k::Escape => view.borrow_mut().close_search(),
                // Enter goes to older matches, Shift+Enter to newer ones.
                k::Return | k::KP_Enter => view.borrow_mut().search_step(false, !shift),
                _ => return glib::Propagation::Proceed,
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
            // Keys belong to the image viewer while it is open.
            if view.borrow().viewer_open() {
                if ev.keyval() == gdk::keys::constants::Escape {
                    view.borrow_mut().close_viewer();
                }
                return glib::Propagation::Stop;
            }
            let (key, mods) = (input::key(ev), mods(ev.state()));
            let binding = view
                .borrow()
                .flow
                .keys
                .get(&keys::name(&key, mods))
                .cloned();
            match binding {
                Some(Binding::Send { send }) => {
                    view.borrow_mut().scroll_to_end();
                    session.input(send.as_bytes());
                    return glib::Propagation::Stop;
                }
                Some(Binding::Command { command }) => {
                    run_command(&command, session.cwd());
                    return glib::Propagation::Stop;
                }
                Some(Binding::Action(action)) if view.borrow_mut().run_action(&action) => {
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
            if let Some(data) = encode(&key, mods, &modes.lock().unwrap()) {
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

    events::pump(session.clone(), output, modes, redraw, proxy);

    {
        // Programs can make the cursor blink (DECSCUSR), whatever the
        // configuration says.
        let view = Rc::downgrade(&view);
        glib::timeout_add_local(std::time::Duration::from_millis(530), move || {
            let Some(view) = view.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if let Ok(v) = view.try_borrow()
                && v.session.read(|term| term.cursor().1)
            {
                v.flow.blink_on.set(!v.flow.blink_on.get());
                v.invalidate();
            }
            glib::ControlFlow::Continue
        });
    }
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Redraw) => {
                raterm::trace!("event");
                let mut v = view.borrow_mut();
                v.redraw.done();
                v.layout();
                raterm::trace!("laid");
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(focused),
                ..
            } => {
                let v = view.borrow();
                v.flow.focused.set(focused);
                v.invalidate();
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
    /// Changes the size of the font: the cells, the grid, the pty, and the
    /// blocks, made again at the new size.
    fn set_zoom(&mut self, zoom: i32) {
        self.flow.zoom = zoom;
        let scale = self.flow.scale();
        let config = &self.flow.config;
        self.font
            .set_size((config.font.size as f64 * scale * pango::SCALE as f64) as i32);
        self.flow.cell = measure(&self.area, &self.font);
        self.baseline = draw::baseline(&self.area, &self.font);
        self.block_style = block_style(config, self.flow.cell.0, scale);
        for view in self.blocks.values() {
            unsafe { view.destroy() };
        }
        self.blocks.clear();
        self.flow.heights.borrow_mut().clear();
        self.flow.table_widths.borrow_mut().clear();
        // The pty gets the new sizes in pixels even if the cells fit the same.
        self.flow.size = (0, 0);
        self.resized(&self.area.allocation());
        self.layout();
    }

    fn resized(&mut self, alloc: &gtk::Allocation) {
        let redraw = self.redraw.clone();
        let (w, h) = (alloc.width() as f64, alloc.height() as f64);
        self.flow.resized(w, h, move || redraw.request());
    }

    fn wheel(&mut self, ev: &gdk::EventScroll) {
        let dy = match ev.direction() {
            gdk::ScrollDirection::Up => -1.0,
            gdk::ScrollDirection::Down => 1.0,
            _ => ev.delta().1,
        };
        let (x, y) = ev.position();
        if self.flow.wheel(dy, mods(ev.state()), x, y) {
            self.layout();
        }
    }

    /// Scrolls the flow to put height `top` of a block in the middle.
    fn reveal_in_block(&mut self, entry: u64, top: f64) {
        self.flow.reveal_in_block(entry, top);
        self.layout();
    }

    fn open_search(&mut self) {
        self.flow.search.get_or_insert_default();
        self.search_entry.show();
        // Selects the last query, to type a new one over it.
        self.search_entry.grab_focus();
    }

    fn close_search(&mut self) {
        if let Some(entry) = self.flow.search.as_ref().and_then(|s| s.block) {
            self.find_in_block(entry, Find::Finish);
        }
        self.flow.search = None;
        self.search_entry.hide();
        self.area.grab_focus();
        self.area.queue_draw();
    }

    /// Goes to the next match, older or newer, or from the end for a new
    /// query (see `Flow::search_step`).
    fn search_step(&mut self, restart: bool, backwards: bool) {
        let query = self.search_entry.text().to_string();
        let blocks = &self.blocks;
        let Some(found) =
            (self.flow).search_step(&query, restart, backwards, |b| blocks.contains_key(&b))
        else {
            return;
        };
        let style = self.search_entry.style_context();
        if found.any {
            style.remove_class("none");
        } else {
            style.add_class("none");
        }
        // Makes the web view of the block, if it was out of view.
        self.layout();
        for (entry, find) in found.finds {
            self.find_in_block(entry, find);
        }
    }

    /// Runs a shortcut's action; false lets the key reach the program.
    fn run_action(&mut self, action: &str) -> bool {
        let Some(action) = self.flow.action(action) else {
            return false;
        };
        match action {
            Action::Layout => self.layout(),
            Action::Copy(which) => self.copy(which),
            Action::Paste(which) => self.paste(which),
            Action::NewWindow => new_window(self.session.cwd()),
            Action::OpenSearch => self.open_search(),
            Action::Zoom(zoom) => self.set_zoom(zoom),
        }
        true
    }

    /// Copies the selection to the clipboard or the primary selection; that
    /// of a block, the web view copies itself.
    fn copy(&self, which: Which) {
        let Some(text) = self.flow.selected_text() else {
            if which == Which::Clipboard
                && let Some(view) = self.flow.block_selected.and_then(|e| self.blocks.get(&e))
            {
                view.copy();
            }
            return;
        };
        GtkClipboard.set(which, &text);
    }

    fn paste(&self, which: Which) {
        let session = self.session.clone();
        GtkClipboard.paste(which, move |text| session.paste(&text));
    }

    fn scroll_to_end(&mut self) {
        if self.flow.scroll != 0 {
            self.flow.scroll = 0;
            self.layout();
        }
    }

    /// Places the web views of the blocks in view, then queues a draw.
    fn layout(&mut self) {
        let (spots, scrolled) = self.flow.layout();
        for spot in &spots {
            if !self.blocks.contains_key(&spot.entry) {
                let view = self.block_view(spot.entry, &spot.block);
                self.fixed.put(&view, spot.x, spot.y);
                view.show();
                self.blocks.insert(spot.entry, view);
            }
            let view = &self.blocks[&spot.entry];
            view.place(spot.x, spot.y, spot.width, spot.height);
            self.fixed.move_(view, spot.x, spot.y);
        }
        // Blocks out of view go; they are made again when they come back.
        self.blocks.retain(|entry, view| {
            let keep = spots.iter().any(|s| s.entry == *entry);
            if !keep {
                unsafe { view.destroy() };
            }
            keep
        });
        if scrolled {
            self.show_indicator();
        }
        self.invalidate();
    }

    /// Asks GTK to draw the rows that changed again, with the margins of
    /// their images (see draw_line): the compositor then takes less of the
    /// window.
    fn invalidate(&self) {
        match self.session.read(|term| self.changed_rows(term)) {
            Some(rows) => rows.into_iter().for_each(|row| self.queue_row(row)),
            None => self.area.queue_draw(),
        }
    }

    /// Asks GTK to draw a row again, with the rows above and below where its
    /// image may spill.
    fn queue_row(&self, row: usize) {
        let ch = self.flow.cell.1;
        let y = PAD_Y + (row as f64 - 1.0) * ch;
        self.area.queue_draw_area(
            0,
            y.floor() as i32,
            self.area.allocated_width(),
            (3.0 * ch).ceil() as i32 + 1,
        );
    }

    /// Shows the scroll indicator, and redraws while it fades.
    fn show_indicator(&self) {
        let scrolled_at = self.flow.scrolled_at.clone();
        let fading = scrolled_at.get().is_some();
        scrolled_at.set(Some(std::time::Instant::now()));
        if fading {
            return;
        }
        let area = self.area.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
            area.queue_draw();
            if scrolled_at
                .get()
                .is_some_and(|t| t.elapsed() > draw::INDICATOR_SHOWN)
            {
                scrolled_at.set(None);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

/// GTK's clipboard and primary selection.
struct GtkClipboard;

impl GtkClipboard {
    fn get(which: Which) -> gtk::Clipboard {
        gtk::Clipboard::get(&match which {
            Which::Clipboard => gdk::SELECTION_CLIPBOARD,
            Which::Primary => gdk::SELECTION_PRIMARY,
        })
    }
}

impl Clipboard for GtkClipboard {
    fn set(&self, which: Which, text: &str) {
        Self::get(which).set_text(text);
    }

    fn paste(&self, which: Which, then: impl FnOnce(String) + 'static) {
        let mut then = Some(then);
        Self::get(which).request_text(move |_, text| {
            if let (Some(text), Some(then)) = (text, then.take()) {
                then(text.to_owned());
            }
        });
    }
}

/// The search bar, at the top right, in the terminal's colors.
fn search_bar(config: &Config) -> gtk::Entry {
    let entry = gtk::Entry::new();
    entry.set_halign(gtk::Align::End);
    entry.set_valign(gtk::Align::Start);
    entry.set_margin_top(6);
    entry.set_margin_end(12);
    entry.set_placeholder_text(Some("Search"));
    // Hidden until opened, through show_all too.
    entry.set_no_show_all(true);
    let t = theme::theme();
    let css = format!(
        "entry {{ font: {}pt {}; color: #{:06x}; background: #{:06x}; border: 1px solid #{:06x}; \
         border-radius: 0; box-shadow: none; padding: 2px 6px; min-height: 0; }} \
         entry.none {{ border-color: #{:06x}; }}",
        config.font.size,
        css_fonts(&config.font.family, "monospace"),
        t.fg,
        t.bg,
        t.palette[8],
        t.palette[1],
    );
    let provider = gtk::CssProvider::new();
    if let Err(e) = provider.load_from_data(css.as_bytes()) {
        eprintln!("raterm: search bar style: {e}");
    }
    entry
        .style_context()
        .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    entry
}
