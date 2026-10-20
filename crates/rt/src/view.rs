use std::borrow::Cow;
use std::cell::RefCell;
use std::thread;

use rt::config::{self, Config};
use rt::pty::Error;
use rt::session::Session;
use rt::term::Event as TermEvent;
use serde::Deserialize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::http::{Response, header::CONTENT_TYPE};
use wry::{PageLoadEvent, WebViewBuilder, WebViewBuilderExtUnix};

enum UserEvent {
    /// A JSON array of terminal events.
    Events(String),
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
}

pub fn run(config: Config) -> Result<(), Error> {
    let (session, reader) = Session::spawn(&config, 80, 24)?;
    let reader = RefCell::new(Some(reader));
    // Served as a script so that the page has it before app.js runs.
    let config_js = format!("const rtConfig = {};", serde_json::to_string(&config)?);

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("rt")
        .with_decorations(config.window.decorations)
        .build(&event_loop)?;

    let pump_session = session.clone();
    let pump_proxy = proxy.clone();
    let title_proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_custom_protocol("rt".into(), move |_, req| {
            serve(req.uri().path(), &config_js)
        })
        .with_url("rt://app/")
        .with_clipboard(true)
        .with_document_title_changed_handler(move |title| {
            let _ = title_proxy.send_event(UserEvent::Title(title));
        })
        // Output read before the page is ready would be lost, so the pty is
        // only drained once the page has loaded; the kernel buffers until then.
        .with_on_page_load_handler(move |event, _| {
            if let PageLoadEvent::Finished = event
                && let Some(reader) = reader.borrow_mut().take()
            {
                let (session, proxy) = (pump_session.clone(), pump_proxy.clone());
                thread::spawn(move || {
                    session.pump(reader, |events| send(&proxy, &events));
                    let _ = proxy.send_event(UserEvent::Exit);
                });
            }
        })
        .with_ipc_handler(move |req| match serde_json::from_str(req.body()) {
            Ok(Message::Input { data }) => session.input(data.as_bytes()),
            Ok(Message::Paste { data }) => session.paste(&data),
            Ok(Message::Resize { cols, rows }) => {
                session.resize(cols, rows, |events| send(&proxy, &events))
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
            Err(e) => eprintln!("rt: bad ipc message: {e}"),
        })
        .build_gtk(window.default_vbox().ok_or("no GTK container")?)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Events(json)) => {
                let _ = webview.evaluate_script(&format!("rt.events({json})"));
            }
            Event::UserEvent(UserEvent::Title(title)) => window.set_title(&title),
            Event::UserEvent(UserEvent::Exit)
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    })
}

fn send(proxy: &EventLoopProxy<UserEvent>, events: &[TermEvent]) {
    let json = serde_json::to_string(events).expect("events serialize");
    let _ = proxy.send_event(UserEvent::Events(json));
}

/// Opens a link from a rich block in the desktop's handler, never in the webview.
fn open_url(url: &str) {
    // Other schemes (file:, custom app handlers) would let terminal output
    // launch arbitrary local handlers on a click.
    if !["http://", "https://", "mailto:"]
        .iter()
        .any(|s| url.starts_with(s))
    {
        return eprintln!("rt: refusing to open {url:?}");
    }
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("rt: xdg-open failed: {e}"),
    }
}

/// Opens another rt window, in the same directory as this one's shell.
fn new_window(cwd: Option<std::path::PathBuf>) {
    let mut command = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe),
        Err(e) => return eprintln!("rt: cannot find own executable: {e}"),
    };
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    match command.spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("rt: cannot open a new window: {e}"),
    }
}

fn serve(path: &str, config_js: &str) -> Response<Cow<'static, [u8]>> {
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
        // Rich blocks inherit this policy: they can show inline styles and
        // data: images, but never load anything from the network.
        .header(
            "Content-Security-Policy",
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'",
        )
        .body(body)
        .unwrap()
}
