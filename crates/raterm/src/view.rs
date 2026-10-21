use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::thread;

use raterm::config::{self, Config};
use raterm::history;
use raterm::media;
use raterm::pty::Error;
use raterm::session::Session;
use raterm::term::{self, Event as TermEvent};
use serde::Deserialize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::http::Response;
use wry::http::header::CONTENT_TYPE;
use wry::{PageLoadEvent, WebViewBuilder, WebViewBuilderExtUnix};

enum UserEvent {
    /// Terminal events wait in the outbox.
    Events,
    Title(String),
    Exit,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Input {
        data: String,
    },
    Paste {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    OpenUrl {
        url: String,
    },
    /// Paste the primary selection (middle click, shift+insert).
    PastePrimary,
    NewWindow,
    /// Entries `from..from + count` of the scrollback.
    Entries {
        from: u64,
        count: usize,
    },
    /// The match of `query` next to offset `at` of `entry`.
    Search {
        query: String,
        entry: Option<u64>,
        at: usize,
        backwards: bool,
    },
}

/// Opens the terminal window, running `command` with `title` as the window
/// title until the program sets its own.
pub fn run(config: Config, command: &[String], title: String) -> Result<(), Error> {
    let (session, reader) = Session::spawn(&config, command, 80, 24)?;
    let reader = RefCell::new(Some(reader));
    let media = media::serve(session.media())?;
    // Served as a script so that the page has it before app.js runs.
    let config_js = format!(
        "const ratermConfig = {};\nconst ratermMedia = {};\nconst ratermWindow = {};",
        serde_json::to_string(&config)?,
        serde_json::to_string(&media.base)?,
        history::WINDOW,
    );
    // Rich blocks inherit this policy: they can show inline styles and data:
    // images, but never load anything from the network; videos of local files
    // come from raterm's own server.
    let csp = format!(
        "default-src 'self'; img-src 'self' data:; media-src data: {}; \
         style-src 'self' 'unsafe-inline'; frame-src 'self' http: https: mailto:",
        media.origin
    );

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let outbox = Outbox {
        events: Arc::default(),
        proxy: event_loop.create_proxy(),
    };
    let window = WindowBuilder::new()
        .with_title(&title)
        .with_decorations(config.window.decorations)
        .build(&event_loop)?;

    let pump_session = session.clone();
    let pump_outbox = outbox.clone();
    let title_proxy = outbox.proxy.clone();
    let pending = outbox.events.clone();
    let webview = WebViewBuilder::new()
        .with_custom_protocol("raterm".into(), move |_, req| {
            serve(req.uri().path(), &config_js, &csp)
        })
        .with_url("raterm://app/")
        .with_clipboard(true)
        .with_navigation_handler(|url| navigate(&url))
        .with_document_title_changed_handler(move |title| {
            let _ = title_proxy.send_event(UserEvent::Title(title));
        })
        // Output read before the page is ready would be lost, so the pty is
        // only drained once the page has loaded; the kernel buffers until then.
        .with_on_page_load_handler(move |event, _| {
            if let PageLoadEvent::Finished = event
                && let Some(reader) = reader.borrow_mut().take()
            {
                let (session, outbox) = (pump_session.clone(), pump_outbox.clone());
                thread::spawn(move || {
                    session.pump(reader, |events| outbox.send(events));
                    let _ = outbox.proxy.send_event(UserEvent::Exit);
                });
            }
        })
        .with_ipc_handler(move |req| match serde_json::from_str(req.body()) {
            Ok(Message::Input { data }) => session.input(data.as_bytes()),
            Ok(Message::Paste { data }) => session.paste(&data),
            Ok(Message::Resize { cols, rows }) => {
                session.resize(cols, rows, |events| outbox.send(events))
            }
            Ok(Message::OpenUrl { url }) => open_url(&url),
            Ok(Message::PastePrimary) => {
                let session = session.clone();
                gtk::Clipboard::get(&gtk::gdk::SELECTION_PRIMARY).request_text(move |_, text| {
                    if let Some(text) = text {
                        session.paste(text);
                    }
                });
            }
            Ok(Message::NewWindow) => new_window(session.cwd()),
            Ok(Message::Entries { from, count }) => {
                session.query(|term| term.entries(from, count), |e| outbox.send(e))
            }
            Ok(Message::Search {
                query,
                entry,
                at,
                backwards,
            }) => session.query(
                |term| term.search(&query, entry.map(|n| (n, at)), backwards),
                |e| outbox.send(e),
            ),
            Err(e) => eprintln!("raterm: bad ipc message: {e}"),
        })
        .build_gtk(window.default_vbox().ok_or("no GTK container")?)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Events) => {
                let events = std::mem::take(&mut *pending.lock().unwrap());
                let json = serde_json::to_string(&events).expect("events serialize");
                let _ = webview.evaluate_script(&format!("raterm.events({json})"));
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

/// Events on their way to the page. While it is busy (a flood), those of
/// several batches pile up here and go in one call, without the entries that
/// the page would drop: evaluating the JSON of every line that scrolled out
/// took longer than everything else the page does.
#[derive(Clone)]
struct Outbox {
    events: Arc<Mutex<Vec<TermEvent>>>,
    proxy: EventLoopProxy<UserEvent>,
}

impl Outbox {
    fn send(&self, events: Vec<TermEvent>) {
        let mut pending = self.events.lock().unwrap();
        let wake = pending.is_empty();
        pending.extend(events);
        term::trim(&mut pending);
        if wake {
            let _ = self.proxy.send_event(UserEvent::Events);
        }
    }
}

/// Only the app and its rich blocks (about:srcdoc) load in the webview. Any
/// other navigation is a link clicked in a block: no listener can catch the
/// click there (the block runs no script), so it is caught here instead.
/// Blocks cannot navigate on their own: the sandbox blocks <meta refresh>.
fn navigate(url: &str) -> bool {
    if url.starts_with("raterm://app/") || url.starts_with("about:") {
        return true;
    }
    open_url(url);
    false
}

/// Opens a link in the desktop's handler, never in the webview.
pub fn open_url(url: &str) {
    // Other schemes (file:, custom app handlers) would let terminal output
    // launch arbitrary local handlers on a click.
    if !["http://", "https://", "mailto:"]
        .iter()
        .any(|s| url.starts_with(s))
    {
        return eprintln!("raterm: refusing to open {url:?}");
    }
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("raterm: xdg-open failed: {e}"),
    }
}

/// Opens another raterm window, in the web view too, in the same directory
/// as this one's shell.
fn new_window(cwd: Option<std::path::PathBuf>) {
    let mut command = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe),
        Err(e) => return eprintln!("raterm: cannot find own executable: {e}"),
    };
    command.arg("--web");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    match command.spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("raterm: cannot open a new window: {e}"),
    }
}

fn serve(path: &str, config_js: &str, csp: &str) -> Response<Cow<'static, [u8]>> {
    let (body, mime): (Cow<'static, [u8]>, _) = match path {
        "/" | "/index.html" => (include_bytes!("../assets/index.html").into(), "text/html"),
        "/style.css" => (include_bytes!("../assets/style.css").into(), "text/css"),
        "/app.js" => (include_bytes!("../assets/app.js").into(), "text/javascript"),
        "/config.js" => (config_js.as_bytes().to_vec().into(), "text/javascript"),
        // Loaded last, so it overrides everything; empty when absent.
        "/user.css" => (
            std::fs::read(config::dir().join("user.css"))
                .unwrap_or_default()
                .into(),
            "text/css",
        ),
        _ => {
            return Response::builder()
                .status(404)
                .body(Cow::Borrowed(&[][..]))
                .unwrap();
        }
    };
    Response::builder()
        .header(CONTENT_TYPE, mime)
        .header("Content-Security-Policy", csp)
        .body(body)
        .unwrap()
}
