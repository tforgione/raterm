//! The view on macOS (see Spec-macOS in the wiki): a `tao` window, the grid
//! drawn with Core Graphics and Core Text in an `NSView` of its own
//! (`draw.rs`), rich blocks in `WKWebView`s over it (`blocks.rs`). What the
//! view decides is `Flow`'s, shared with Linux; input comes from tao's
//! events.

mod blocks;
mod draw;
mod input;
mod menu;
mod search;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use raterm::config::{Binding, Config};
use raterm::pty::Error;
use raterm::rich::{Kind, Rendered};
use raterm::session::Session;
use raterm::term::Modes;
use tao::dpi::LogicalSize;
use tao::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::WindowExtMacOS;
use tao::window::{Window, WindowBuilder};

use crate::native::blocks::{
    BlockView, CLEAR_SELECTION, MEASURE, Told, background, block_style, find_script, told,
    viewer_document,
};
use crate::native::events::{self, Redraw, UserEvent};
use crate::native::flow::{Action, Find, Flow};
use crate::native::keys::{self, Mods, encode};
use crate::native::paint::INDICATOR_SHOWN;
use crate::native::select::{Selection, Unit};
use crate::native::system::{Clipboard, Which, new_window, open_url};
use crate::native::theme;
use blocks::WebView;
use draw::{Font, Grid};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// How often a blinking cursor blinks, as on Linux.
const BLINK: Duration = Duration::from_millis(530);
/// Two clicks closer than this, on the same cell, make a double click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

struct View {
    flow: Flow,
    session: Arc<Session>,
    window: Window,
    grid: Grid,
    font: Font,
    /// Web views of the blocks in view.
    blocks: HashMap<u64, WebView>,
    /// The image shown full window.
    viewer: Option<WebView>,
    redraw: Arc<Redraw>,
    /// This view, for the handlers of the web views it makes.
    me: Weak<RefCell<View>>,
    block_style: String,
    media: raterm::media::Server,
    /// The modifiers held, the pointer in points, and whether the left
    /// button is down.
    mods: Mods,
    pointer: (f64, f64),
    left_down: bool,
    /// The last click, and how many came in a row (2 for a double click).
    last_click: Option<(Instant, (f64, f64), u8)>,
    /// When the cursor blinks next.
    next_blink: Instant,
    /// The text of the search field, as last searched.
    search_query: String,
    /// The search field, while it is open.
    search_field: Option<search::Field>,
    /// The input method typed the text of the key being pressed: tao tells
    /// it (`insertText:`) before the key itself, whose text is the same.
    typed: bool,
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
    let font = Font::new(&config.font.family, config.font.size as f64);
    let cell = font.cell();
    let redraw = Redraw::new(proxy.clone());
    let modes = Arc::new(Mutex::new(Modes::default()));
    // The program starts with the cell's size, which it may ask for
    // (`CSI 16 t`) before the window is first sized.
    let (session, output) = Session::spawn(&config, command, 80, 24, cell)?;
    // Local videos and audio are streamed to the web views of blocks.
    let media = raterm::media::serve(session.media())?;
    let view = Rc::new_cyclic(|me: &Weak<RefCell<View>>| {
        let drawer = me.clone();
        let grid = Grid::new(
            &window,
            Box::new(move |surface, width, height| {
                if let Some(view) = drawer.upgrade()
                    && let Ok(view) = view.try_borrow()
                {
                    let own = |c: char| view.font.has_glyph(c);
                    let mut canvas = surface.canvas(&view.font);
                    view.flow.draw_frame(&mut canvas, width, height, own);
                }
            }),
        );
        RefCell::new(View {
            flow: Flow::new(session.clone(), config.clone(), modes.clone(), cell),
            session: session.clone(),
            window,
            grid,
            font,
            blocks: HashMap::new(),
            viewer: None,
            redraw: redraw.clone(),
            me: me.clone(),
            block_style: block_style(&config, cell.0, 1.0),
            media,
            mods: Mods::default(),
            pointer: (0.0, 0.0),
            left_down: false,
            last_click: None,
            next_blink: Instant::now() + BLINK,
            search_query: String::new(),
            search_field: None,
            typed: false,
        })
    });
    menu::install();
    // tao sends no `Resized` for the first size of the window on macOS.
    view.borrow_mut().resized();
    events::pump(session, output, modes, redraw, proxy);

    event_loop.run(move |event, _, control_flow| {
        let mut v = view.borrow_mut();
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => v.tick(),
            Event::UserEvent(UserEvent::Redraw) => {
                v.redraw.done();
                v.layout();
            }
            Event::UserEvent(UserEvent::Title(set)) => {
                v.window
                    .set_title(if set.is_empty() { &title } else { &set })
            }
            Event::UserEvent(UserEvent::Exit) => *control_flow = ControlFlow::Exit,
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => v.resized(),
                WindowEvent::Focused(focused) => {
                    v.flow.focused.set(focused);
                    v.grid.invalidate();
                }
                WindowEvent::ModifiersChanged(state) => v.mods = input::mods(state),
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed =>
                {
                    v.key(&event)
                }
                WindowEvent::ReceivedImeText(text) => {
                    v.scroll_to_end();
                    v.session.input(text.as_bytes());
                    v.typed = true;
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let p = position.to_logical::<f64>(v.window.scale_factor());
                    v.moved(p.x, p.y);
                }
                WindowEvent::MouseInput { state, button, .. } => match state {
                    ElementState::Pressed => v.pressed(button),
                    _ => v.released(button),
                },
                WindowEvent::MouseWheel { delta, .. } => {
                    // tao's wheel goes up for positive deltas, the flow's down.
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => -y as f64,
                        MouseScrollDelta::PixelDelta(p) => {
                            -p.to_logical::<f64>(v.window.scale_factor()).y / v.flow.cell.1
                        }
                        _ => 0.0,
                    };
                    let (x, y) = v.pointer;
                    let mods = v.mods;
                    if v.flow.wheel(dy, mods, x, y) {
                        v.layout();
                    }
                }
                _ => {}
            },
            _ => {}
        }
        // Wakes up to blink the cursor, and to fade the scroll indicator.
        let fading = v.flow.scrolled_at.get().is_some();
        let next = if fading {
            Instant::now() + Duration::from_millis(40)
        } else {
            v.next_blink
        };
        if *control_flow != ControlFlow::Exit {
            *control_flow = ControlFlow::WaitUntil(next);
        }
    })
}

impl View {
    /// A timer went off: the cursor blinks, the scroll indicator fades.
    fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.next_blink {
            self.next_blink = now + BLINK;
            // Programs can make the cursor blink (DECSCUSR), whatever the
            // configuration says.
            if self.session.read(|term| term.cursor().1) {
                self.flow.blink_on.set(!self.flow.blink_on.get());
                self.grid.invalidate();
            }
        }
        if let Some(at) = self.flow.scrolled_at.get() {
            if at.elapsed() > INDICATOR_SHOWN {
                self.flow.scrolled_at.set(None);
            }
            self.grid.invalidate();
        }
    }

    /// The window's size, in points, sizes the grid.
    fn resized(&mut self) {
        let size: LogicalSize<f64> = self
            .window
            .inner_size()
            .to_logical(self.window.scale_factor());
        let redraw = self.redraw.clone();
        self.flow
            .resized(size.width, size.height, move || redraw.request());
    }

    fn key(&mut self, event: &tao::event::KeyEvent) {
        let mods = self.mods;
        let key = input::key(event, mods);
        let typed = std::mem::take(&mut self.typed);
        // Keys belong to the image viewer while it is open.
        if self.viewer.is_some() {
            if key == Some(keys::Key::Named(keys::Named::Escape)) {
                self.close_viewer();
            }
            return;
        }
        if let Some(key) = &key {
            match self.flow.keys.get(&keys::name(key, mods)).cloned() {
                Some(Binding::Send { send }) => {
                    self.scroll_to_end();
                    return self.session.input(send.as_bytes());
                }
                Some(Binding::Action(action)) if self.run_action(&action) => return,
                _ => {}
            }
            // Typed already, Option+key as the layout composes it too.
            if typed {
                return;
            }
            let modes = *self.flow.modes.lock().unwrap();
            if let Some(data) = encode(key, mods, &modes) {
                self.scroll_to_end();
                return self.session.input(data.as_bytes());
            }
        }
        // Text, which the input method may have composed. Cmd+key that is
        // not a shortcut types nothing, as in other macOS terminals.
        if let Some(text) = event.text.filter(|_| !mods.logo) {
            self.scroll_to_end();
            self.session.input(text.as_bytes());
        }
    }

    fn pressed(&mut self, button: MouseButton) {
        let (x, y) = self.pointer;
        let mods = self.mods;
        let button = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        // Clicks in a row, on the same spot: 2 for a double click.
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, (px, py), n))
                if now - at < MULTI_CLICK && (px - x).abs() < 4.0 && (py - y).abs() < 4.0 =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, (x, y), count));
        let link = (button == 0 && mods.ctrl)
            .then(|| self.flow.link_at(x, y))
            .flatten();
        if let Some(cell) = self.flow.reported_cell(mods, x, y) {
            // No selection to tell a Ctrl+click on a link apart from.
            if let Some(url) = link {
                return open_url(&url);
            }
            // The program gets the click: no selection.
            if count == 1 {
                self.flow.pressed = Some(button);
                self.flow.last_cell = Some(cell);
                self.flow.send_mouse(button, mods, cell, false);
            }
            return;
        }
        if button != 0 {
            // No primary selection on macOS: the middle button pastes
            // nothing.
            return;
        }
        self.left_down = true;
        let unit = match count {
            2 => Unit::Word,
            3 => Unit::Line,
            _ => Unit::Cell,
        };
        self.clear_selections(None);
        let block = (mods.ctrl && unit == Unit::Cell).then_some(self.flow.size.0 as usize);
        self.flow.link = link.filter(|_| unit == Unit::Cell);
        if let Some(pos) = self.flow.pos_at(x, y, block.is_some()) {
            self.flow.selection = Some(Selection {
                anchor: pos,
                head: pos,
                unit,
                block,
            });
            self.grid.invalidate();
        }
    }

    fn moved(&mut self, x: f64, y: f64) {
        self.pointer = (x, y);
        if self.flow.report_motion(self.mods, x, y) || !self.left_down {
            return;
        }
        let block = self.flow.selection.is_some_and(|s| s.block.is_some());
        if let Some(pos) = self.flow.pos_at(x, y, block)
            && let Some(selection) = &mut self.flow.selection
            && selection.head != pos
        {
            selection.head = pos;
            self.grid.invalidate();
        }
    }

    fn released(&mut self, button: MouseButton) {
        let (x, y) = self.pointer;
        if let Some(pressed) = self.flow.pressed.take() {
            if let Some(cell) = self.flow.screen_cell(x, y).or(self.flow.last_cell) {
                self.flow.send_mouse(pressed, self.mods, cell, true);
            }
            return;
        }
        if button == MouseButton::Left {
            self.left_down = false;
            let link = self.flow.link.take();
            if self.flow.selection.is_some_and(|s| s.is_empty()) {
                self.flow.selection = None;
                self.grid.invalidate();
                if let Some(url) = link {
                    open_url(&url);
                }
            }
        }
    }

    /// Changes the size of the font: the cells, the grid, the pty, and the
    /// blocks, made again at the new size.
    fn set_zoom(&mut self, zoom: i32) {
        self.flow.zoom = zoom;
        let scale = self.flow.scale();
        self.font = self.font.sized(self.flow.config.font.size as f64 * scale);
        self.flow.cell = self.font.cell();
        self.block_style = block_style(&self.flow.config, self.flow.cell.0, scale);
        for view in self.blocks.values() {
            view.remove();
        }
        self.blocks.clear();
        self.flow.heights.borrow_mut().clear();
        self.flow.table_widths.borrow_mut().clear();
        // The pty gets the new sizes in pixels even if the cells fit the same.
        self.flow.size = (0, 0);
        self.resized();
        self.layout();
    }

    fn open_search(&mut self) {
        self.flow.search.get_or_insert_default();
        if let Some(field) = &self.search_field {
            return field.focus();
        }
        let me = self.me.clone();
        let on = Box::new(move |typed| {
            let Some(view) = me.upgrade() else { return };
            let Ok(mut view) = view.try_borrow_mut() else {
                return;
            };
            match typed {
                search::Typed::Changed(text) => view.search_step(&text, true, true),
                // Enter goes to older matches, up the terminal.
                search::Typed::Enter { text, shift } => view.search_step(&text, false, !shift),
                search::Typed::Escape => view.close_search(),
            }
        });
        self.search_field = Some(search::Field::new(self.grid.superview(), on));
    }

    fn close_search(&mut self) {
        if let Some(entry) = self.flow.search.as_ref().and_then(|s| s.block) {
            self.find_in_block(entry, Find::Finish);
        }
        self.flow.search = None;
        self.grid.invalidate();
        if let Some(field) = self.search_field.take() {
            field.remove(self.window.ns_view());
        }
    }

    /// Goes to the next match of `query`, older or newer, or from the end
    /// for a new query (see `Flow::search_step`).
    fn search_step(&mut self, query: &str, restart: bool, backwards: bool) {
        query.clone_into(&mut self.search_query);
        let blocks = &self.blocks;
        let Some(found) =
            (self.flow).search_step(query, restart, backwards, |b| blocks.contains_key(&b))
        else {
            return;
        };
        if let Some(field) = &self.search_field {
            field.set_found(found.any);
        }
        // Makes the web view of the block, if it was out of view.
        self.layout();
        for (entry, find) in found.finds {
            self.find_in_block(entry, find);
        }
    }

    fn find_in_block(&self, entry: u64, find: Find) {
        if let Some(view) = self.blocks.get(&entry) {
            view.run(&find_script(&find));
        }
    }

    /// Clears the selection of the grid and those of the blocks, but the
    /// block `keep`.
    fn clear_selections(&mut self, keep: Option<u64>) {
        if self.flow.selection.take().is_some() {
            self.grid.invalidate();
        }
        self.flow.block_selected = None;
        for (entry, view) in &self.blocks {
            if Some(*entry) != keep {
                view.run(CLEAR_SELECTION);
            }
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

    /// Copies the selection to the clipboard; that of a block, the web view
    /// copies itself.
    fn copy(&self, which: Which) {
        let Some(text) = self.flow.selected_text() else {
            if which == Which::Clipboard
                && let Some(view) = self.flow.block_selected.and_then(|e| self.blocks.get(&e))
            {
                view.copy();
            }
            return;
        };
        MacClipboard.set(which, &text);
    }

    fn paste(&self, which: Which) {
        let session = self.session.clone();
        MacClipboard.paste(which, move |text| session.paste(&text));
    }

    fn scroll_to_end(&mut self) {
        if self.flow.scroll != 0 {
            self.flow.scroll = 0;
            self.layout();
        }
    }

    /// Places the web views of the blocks in view, then draws.
    fn layout(&mut self) {
        let (spots, scrolled) = self.flow.layout();
        for spot in &spots {
            if !self.blocks.contains_key(&spot.entry) {
                let view = self.block_view(spot.entry, &spot.block);
                self.blocks.insert(spot.entry, view);
            }
            self.blocks[&spot.entry].place(spot.x, spot.y, spot.width, spot.height);
        }
        // Blocks out of view go; they are made again when they come back.
        self.blocks.retain(|entry, view| {
            let keep = spots.iter().any(|s| s.entry == *entry);
            if !keep {
                view.remove();
            }
            keep
        });
        if scrolled {
            // The loop redraws while the indicator fades (see `tick`).
            self.flow.scrolled_at.set(Some(Instant::now()));
        }
        self.grid.invalidate();
    }

    /// A web view for a block: its document is the block's content alone,
    /// without scripts nor network, links open in the desktop's handler.
    fn block_view(&mut self, entry: u64, block: &Rendered) -> WebView {
        let html = self.flow.document(block, &self.media, &self.block_style);
        let opaque = (self.flow.config.opaque_blocks()).then(|| background(&self.flow.config));
        let image = matches!(block.kind, Kind::Img | Kind::Svg).then(|| block.clone());
        let me = self.me.clone();
        let redraw = self.redraw.clone();
        let heights = Rc::downgrade(&self.flow.heights);
        let widths = Rc::downgrade(&self.flow.table_widths);
        let events = Box::new(move |event| match event {
            blocks::Event::Loaded => {
                let Some(this) = me.upgrade() else { return };
                let Ok(this) = this.try_borrow() else { return };
                let Some(view) = this.blocks.get(&entry) else {
                    return;
                };
                view.run(MEASURE);
                // A block made while it holds the current match searches
                // once loaded.
                if let Some(search) = &this.flow.search
                    && search.block == Some(entry)
                {
                    let query = this.search_query.clone();
                    view.run(&find_script(&Find::Start {
                        query,
                        backwards: true,
                    }));
                }
            }
            blocks::Event::Title(title) => {
                let (Some(heights), Some(widths)) = (heights.upgrade(), widths.upgrade()) else {
                    return;
                };
                match told(entry, &title, &heights, &widths) {
                    Some(Told::Measured) => redraw.request(),
                    Some(Told::Found(top)) => {
                        if let Some(this) = me.upgrade()
                            && let Ok(mut this) = this.try_borrow_mut()
                        {
                            this.flow.reveal_in_block(entry, top);
                            this.layout();
                        }
                    }
                    None => {}
                }
            }
            blocks::Event::Click => {
                let Some(this) = me.upgrade() else { return };
                let Ok(mut this) = this.try_borrow_mut() else {
                    return;
                };
                match &image {
                    Some(image) => this.open_viewer(image),
                    None => {
                        // One selection at a time: one in a block clears the
                        // others.
                        this.clear_selections(Some(entry));
                        this.flow.block_selected = Some(entry);
                    }
                }
            }
        });
        let view = WebView::new(self.grid.superview(), opaque, events);
        view.load(&html);
        view
    }

    /// Shows an image full window over the terminal, until a click or
    /// Escape.
    fn open_viewer(&mut self, image: &Rendered) {
        self.close_viewer();
        let me = self.me.clone();
        let on_click = Box::new(move || {
            if let Some(view) = me.upgrade()
                && let Ok(mut view) = view.try_borrow_mut()
            {
                view.close_viewer();
            }
        });
        let viewer = WebView::viewer(self.grid.superview(), on_click);
        viewer.load(&viewer_document(image));
        self.viewer = Some(viewer);
    }

    fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            viewer.remove();
        }
    }
}

/// The general pasteboard; macOS has no primary selection.
struct MacClipboard;

impl Clipboard for MacClipboard {
    fn set(&self, which: Which, text: &str) {
        if which == Which::Primary {
            return;
        }
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
    }

    fn paste(&self, which: Which, then: impl FnOnce(String) + 'static) {
        if which == Which::Primary {
            return;
        }
        let board = NSPasteboard::generalPasteboard();
        if let Some(text) = board.stringForType(unsafe { NSPasteboardTypeString }) {
            then(text.to_string());
        }
    }
}
