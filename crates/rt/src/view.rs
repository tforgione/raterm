use std::borrow::Cow;
use std::cell::RefCell;
use std::thread;

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
    Exit,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Input { data: String },
    Paste { data: String },
    Resize { cols: u16, rows: u16 },
    OpenUrl { url: String },
}

pub fn run(shell: &str) -> Result<(), Error> {
    let (session, reader) = Session::spawn(shell, 80, 24)?;
    let reader = RefCell::new(Some(reader));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new().with_title("rt").build(&event_loop)?;

    let pump_session = session.clone();
    let pump_proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_custom_protocol("rt".into(), |_, req| serve(req.uri().path()))
        .with_url("rt://app/")
        .with_clipboard(true)
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
            Err(e) => eprintln!("rt: bad ipc message: {e}"),
        })
        .build_gtk(window.default_vbox().ok_or("no GTK container")?)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Events(json)) => {
                let _ = webview.evaluate_script(&format!("rt.events({json})"));
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

fn serve(path: &str) -> Response<Cow<'static, [u8]>> {
    let (body, mime) = match path {
        "/" | "/index.html" => (include_str!("../assets/index.html"), "text/html"),
        "/style.css" => (include_str!("../assets/style.css"), "text/css"),
        "/app.js" => (include_str!("../assets/app.js"), "text/javascript"),
        _ => ("", "text/plain"),
    };
    Response::builder()
        .status(if body.is_empty() { 404 } else { 200 })
        .header(CONTENT_TYPE, mime)
        // Rich blocks inherit this policy: they can show inline styles and
        // data: images, but never load anything from the network.
        .header(
            "Content-Security-Policy",
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'",
        )
        .body(Cow::Borrowed(body.as_bytes()))
        .unwrap()
}
