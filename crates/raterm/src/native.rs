//! The native view, prototype of M6a (see Spec-Native-grid in the wiki): the
//! grid is drawn by GTK with Pango straight from the core's state, and rich
//! blocks are child web views placed in the flow. No selection, search nor
//! mouse reporting yet.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use raterm::config::{self, Config};
use raterm::history::Entry;
use raterm::pty::Error;
use raterm::rich::{Kind, Rendered};
use raterm::session::Session;
use raterm::term::{Event as TermEvent, Line, Modes, Run, Terminal};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use webkit2gtk::{
    LoadEvent, NavigationPolicyDecisionExt, PolicyDecisionExt, PolicyDecisionType, SettingsExt,
    URIRequestExt, WebContext, WebView, WebViewExt,
};

enum UserEvent {
    /// New output: lay the flow out again and draw it at the next frame.
    Redraw,
    Title(String),
    Exit,
}

struct Theme {
    fg: u32,
    bg: u32,
    cursor: u32,
    palette: [u32; 16],
}

/// The default theme of style.css, with the colors of `user.css` over it:
/// until the palette has a place in config.toml (Spec-Native-grid), the
/// native view reads the same variables as the page.
fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let mut theme = Theme {
            fg: 0xd8dee9,
            bg: 0x151618,
            cursor: 0xd8dee9,
            palette: [
                0x2e3440, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd8dee9,
                0x5c6370, 0xf08088, 0xb5e890, 0xf5d58e, 0x82c4ff, 0xda9cf0, 0x7fd4de, 0xffffff,
            ],
        };
        let css = std::fs::read_to_string(config::dir().join("user.css")).unwrap_or_default();
        for decl in css.split([';', '{', '}']) {
            let Some((name, value)) = decl.split_once(':') else {
                continue;
            };
            let Some(rgb) = value
                .trim()
                .strip_prefix('#')
                .and_then(|h| u32::from_str_radix(h, 16).ok())
            else {
                continue;
            };
            match name.trim().strip_prefix("--") {
                Some("fg") => theme.fg = rgb,
                Some("bg") => theme.bg = rgb,
                Some("cursor") => theme.cursor = rgb,
                Some(c) => {
                    if let Some(i) = c.strip_prefix('c').and_then(|i| i.parse::<usize>().ok())
                        && i < 16
                    {
                        theme.palette[i] = rgb;
                    }
                }
                None => {}
            }
        }
        theme
    })
}
const PAD_X: f64 = 4.0;
const PAD_Y: f64 = 2.0;
/// Rows a block takes until its web view has measured it.
const BLOCK_ROWS: usize = 3;

/// One row of the flow, as laid out for drawing.
enum Visual<'a> {
    /// Cells `start..` of a line, up to the width of the view.
    Text { line: &'a Line, start: usize },
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

/// Width and height of a cell of the font, in pixels.
fn measure(area: &gtk::DrawingArea, font: &pango::FontDescription) -> (f64, f64) {
    let layout = area.create_pango_layout(Some(&"X".repeat(100)));
    layout.set_font_description(Some(font));
    let (w, h) = layout.size();
    let scale = pango::SCALE as f64;
    (w as f64 / scale / 100.0, (h as f64 / scale).ceil())
}

/// Cells a run takes.
fn cells(run: &Run) -> usize {
    let wide = if run.style.class.contains("wide") {
        2
    } else {
        1
    };
    run.text.chars().count() * wide
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

    fn draw(&self, cr: &cairo::Context) {
        set_color(cr, theme().bg, 1.0);
        let _ = cr.paint();
        let style = self.area.style_context();
        self.session.read(|term| {
            for (i, visual) in self.visuals(term).iter().enumerate() {
                let y = PAD_Y + i as f64 * self.cell.1;
                if let Visual::Text { line, start } = visual {
                    self.draw_line(cr, &style, line, *start, y);
                }
            }
        });
    }

    /// Draws cells `start..start + cols` of a line at height `y`.
    fn draw_line(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        line: &Line,
        start: usize,
        y: f64,
    ) {
        let (cw, ch) = self.cell;
        let cols = self.size.0 as usize;
        let mut col = 0;
        for run in &line.runs {
            let n = cells(run);
            if col + n <= start {
                col += n;
                continue;
            }
            if col >= start + cols {
                break;
            }
            let class = &run.style.class;
            let cursor = class.contains("cursor");
            let mut fg = color(run.style.fg.as_deref()).unwrap_or(theme().fg);
            let mut bg = color(run.style.bg.as_deref());
            if cursor {
                (fg, bg) = (theme().bg, Some(theme().cursor));
            }
            let wide = if class.contains("wide") { 2 } else { 1 };
            // The chars of the run in this row, with their cell.
            let chars: Vec<(usize, char)> = run
                .text
                .chars()
                .enumerate()
                .map(|(i, c)| (col + i * wide, c))
                .filter(|&(x, _)| x >= start && x < start + cols)
                .collect();
            col += n;
            let Some(&(first, _)) = chars.first() else {
                continue;
            };
            let x = PAD_X + (first - start) as f64 * cw;
            if let Some(bg) = bg {
                set_color(cr, bg, 1.0);
                cr.rectangle(x, y, chars.len() as f64 * wide as f64 * cw, ch);
                let _ = cr.fill();
            }
            if class.split(' ').any(|c| c == "h") {
                continue;
            }
            let attrs = pango::AttrList::new();
            attrs.insert(pango::AttrColor::new_foreground(
                ((fg >> 16) as u16) * 257,
                ((fg >> 8 & 0xff) as u16) * 257,
                ((fg & 0xff) as u16) * 257,
            ));
            for c in class.split(' ') {
                match c {
                    "b" => attrs.insert(pango::AttrInt::new_weight(pango::Weight::Bold)),
                    "i" => attrs.insert(pango::AttrInt::new_style(pango::Style::Italic)),
                    "u" => attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single)),
                    "s" => attrs.insert(pango::AttrInt::new_strikethrough(true)),
                    "d" => attrs.insert(pango::AttrInt::new_foreground_alpha(0x9999)),
                    _ => {}
                }
            }
            let layout = self.area.create_pango_layout(None);
            layout.set_font_description(Some(&self.font));
            layout.set_attributes(Some(&attrs));
            if run.text.is_ascii() {
                let text: String = chars.iter().map(|&(_, c)| c).collect();
                layout.set_text(&text);
                gtk::render_layout(style, cr, x, y, &layout);
            } else {
                // Glyphs of fallback fonts have other widths: each goes to
                // its own cell, so that the rest of the line stays on the grid.
                for &(cell, c) in &chars {
                    layout.set_text(c.encode_utf8(&mut [0; 4]));
                    gtk::render_layout(style, cr, PAD_X + (cell - start) as f64 * cw, y, &layout);
                }
            }
        }
    }

    /// A web view for a block: its document is the block's content alone,
    /// without scripts nor network, links open in the desktop's handler.
    fn block_view(&mut self, entry: u64, block: &Rendered) -> WebView {
        let context = self.context.get_or_insert_with(WebContext::new);
        let view = WebView::with_context(context);
        if let Some(settings) = WebViewExt::settings(&view) {
            // Scripts of the document never run; raterm's own (measuring the
            // height) still can, from the API.
            settings.set_enable_javascript_markup(false);
        }
        // Opaque: WebKit repaints only what changes (a selection), and a
        // transparent view lost the rest of the block.
        let bg = |shift: u32| ((theme().bg >> shift) & 0xff) as f64 / 255.0;
        view.set_background_color(&gdk::RGBA::new(bg(16), bg(8), bg(0), 1.0));
        view.set_can_focus(false);
        // The wheel scrolls the terminal, over blocks too.
        let me = self.me.clone();
        view.connect_scroll_event(move |_, ev| {
            if let Some(view) = me.upgrade() {
                view.borrow_mut().wheel(ev);
            }
            glib::Propagation::Stop
        });
        view.connect_decide_policy(|_, decision, kind| {
            if kind != PolicyDecisionType::NavigationAction {
                return false;
            }
            let uri = decision
                .downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action())
                .and_then(|a| a.request())
                .and_then(|r| r.uri());
            match uri {
                Some(uri) if !uri.starts_with("about:") && !uri.starts_with("data:") => {
                    crate::view::open_url(&uri);
                    decision.ignore();
                    true
                }
                _ => false,
            }
        });
        // The height comes back through the document's title: reading the
        // result of a script would take the javascriptcore crate.
        view.connect_load_changed(|view, event| {
            if event == LoadEvent::Finished {
                view.evaluate_javascript(
                    // The body: the document is never smaller than the view.
                    "document.title = Math.ceil(document.body.getBoundingClientRect().height)",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        });
        let redraw = self.redraw.clone();
        let heights = Rc::downgrade(&self.heights);
        view.connect_title_notify(move |view| {
            let height = view.title().and_then(|t| t.parse::<f64>().ok());
            if let (Some(h), Some(heights)) = (height, heights.upgrade()) {
                heights.borrow_mut().insert(entry, h);
                redraw.request();
            }
        });
        view.load_html(&document(block, &self.block_style), None);
        view
    }
}

/// The stylesheet of blocks, the same as `richStyle` in app.js.
fn block_style(config: &Config, cell_width: f64) -> String {
    let t = theme();
    let vars: String = (0..16)
        .map(|i| format!("--c{i}: #{:06x}; ", t.palette[i]))
        .collect();
    let columns = config.markdown.max_width;
    let text_width = if columns > 0 {
        format!("{}px", columns as f64 * cell_width)
    } else {
        "none".into()
    };
    format!(
        r#":root {{ --fg: #{fg:06x}; --bg: #{bg:06x}; --inline-code: #e8857f; {vars} color-scheme: dark; }}
html, body {{ margin: 0; color: var(--fg); background: var(--bg); overflow: hidden; }}
body {{ font: {size}pt/1.5 {family}; padding: 4px 0; overflow-x: auto; }}
a {{ color: var(--c4); }}
pre, code {{ font-family: {mono}; font-size: 0.9em; }}
:not(pre) > code {{ color: var(--inline-code); background: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.1); border-radius: 4px; padding: 0.1em 0.35em;
  -webkit-box-decoration-break: clone; box-decoration-break: clone; }}
pre {{ padding: 8px; background: rgba(255, 255, 255, 0.05); overflow-x: auto; }}
.comment {{ color: var(--c8); font-style: italic; }}
.string {{ color: var(--c2); }}
.constant {{ color: var(--c3); }}
.keyword, .storage {{ color: var(--c5); }}
.keyword.operator {{ color: inherit; }}
.entity.name.function, .support.function {{ color: var(--c4); }}
.entity.name.type, .entity.name.class, .support.type, .support.class {{ color: var(--c6); }}
.entity.name.tag {{ color: var(--c1); }}
.entity.other.attribute-name {{ color: var(--c3); }}
table {{ border-collapse: collapse; margin: 8px 0; }}
th, td {{ border: 1px solid var(--c8); padding: 4px 10px; text-align: left; vertical-align: top; }}
th {{ background: rgba(255, 255, 255, 0.08); }}
tbody tr:nth-child(even) {{ background: rgba(255, 255, 255, 0.04); }}
img {{ max-width: 100%; }}
body.md > * {{ max-width: {text_width}; margin-left: auto; margin-right: auto; }}
body.md > table {{ max-width: none; margin: 8px auto; }}
"#,
        fg = t.fg,
        bg = t.bg,
        size = config.blocks.font_size,
        family = css_fonts(&config.blocks.font_family, "sans-serif"),
        mono = css_fonts(&config.font.family, "monospace"),
    )
}

/// A CSS font-family from configured names, quoted (`fontFamily` in app.js).
fn css_fonts(names: &str, fallback: &str) -> String {
    const GENERIC: [&str; 6] = [
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
    ];
    names
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| {
            if GENERIC.contains(&n) {
                n.to_owned()
            } else {
                format!("{n:?}")
            }
        })
        .chain([fallback.to_owned()])
        .collect::<Vec<_>>()
        .join(", ")
}

/// The document of a block, as the page's iframes have it: no network, no
/// script.
fn document(block: &Rendered, style: &str) -> String {
    let body = match block.kind {
        Kind::Img | Kind::Svg => format!(r#"<img src="{}">"#, block.content),
        _ => block.content.clone(),
    };
    let kind = format!("{:?}", block.kind).to_lowercase();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
<style>{style}</style></head><body class="{kind}">{body}</body></html>"#
    )
}

fn set_color(cr: &cairo::Context, rgb: u32, alpha: f64) {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
    cr.set_source_rgba(c(16), c(8), c(0), alpha);
}

/// A color of the core (`var(--c1)`, `#rrggbb`, `var(--fg)`), as RGB.
fn color(spec: Option<&str>) -> Option<u32> {
    let spec = spec?;
    if let Some(hex) = spec.strip_prefix('#') {
        return u32::from_str_radix(hex, 16).ok();
    }
    match spec.strip_prefix("var(--")?.strip_suffix(')')? {
        "fg" => Some(theme().fg),
        "bg" => Some(theme().bg),
        c => theme()
            .palette
            .get(c.strip_prefix('c')?.parse::<usize>().ok()?)
            .copied(),
    }
}

/// The sequence a key sends, the same as `encode` in app.js; `None` for text,
/// which goes through the input method.
fn encode(ev: &gdk::EventKey, modes: &Modes) -> Option<String> {
    use gdk::keys::constants as k;
    let state = ev.state();
    let (shift, alt, ctrl) = (
        state.contains(gdk::ModifierType::SHIFT_MASK),
        state.contains(gdk::ModifierType::MOD1_MASK),
        state.contains(gdk::ModifierType::CONTROL_MASK),
    );
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |fin: &str| {
        if m > 1 {
            format!("\x1b[1;{m}{fin}")
        } else if modes.app_cursor {
            format!("\x1bO{fin}")
        } else {
            format!("\x1b[{fin}")
        }
    };
    let tilde = |n: u8| {
        if m > 1 {
            format!("\x1b[{n};{m}~")
        } else {
            format!("\x1b[{n}~")
        }
    };
    let ss3 = |fin: &str| {
        if m > 1 {
            format!("\x1b[1;{m}{fin}")
        } else {
            format!("\x1bO{fin}")
        }
    };
    let esc = if alt { "\x1b" } else { "" };
    let key = ev.keyval();
    Some(match key {
        k::Up => csi("A"),
        k::Down => csi("B"),
        k::Right => csi("C"),
        k::Left => csi("D"),
        k::Home => csi("H"),
        k::End => csi("F"),
        k::Insert => tilde(2),
        k::Delete => tilde(3),
        k::Page_Up => tilde(5),
        k::Page_Down => tilde(6),
        k::F1 => ss3("P"),
        k::F2 => ss3("Q"),
        k::F3 => ss3("R"),
        k::F4 => ss3("S"),
        k::F5 => tilde(15),
        k::F6 => tilde(17),
        k::F7 => tilde(18),
        k::F8 => tilde(19),
        k::F9 => tilde(20),
        k::F10 => tilde(21),
        k::F11 => tilde(23),
        k::F12 => tilde(24),
        k::Return | k::KP_Enter => format!("{esc}\r"),
        k::BackSpace => format!("{esc}{}", if ctrl { "\x08" } else { "\x7f" }),
        k::Tab => format!("{esc}\t"),
        k::ISO_Left_Tab => "\x1b[Z".into(),
        k::Escape => "\x1b".into(),
        _ => {
            let c = key.to_unicode()?;
            if ctrl {
                let code = match c {
                    ' ' => 0,
                    '/' => 0x1f,
                    c if ('@'..='_').contains(&c.to_ascii_uppercase()) => {
                        c.to_ascii_uppercase() as u8 - 0x40
                    }
                    _ => return None,
                };
                format!("{esc}{}", code as char)
            } else if alt {
                format!("\x1b{c}")
            } else {
                return None;
            }
        }
    })
}
