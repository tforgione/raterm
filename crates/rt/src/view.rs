use std::borrow::Cow;
use std::cell::RefCell;
use std::io::Read;
use std::thread;

use rt::pty::{Error, Pty, decode_utf8};
use serde::Deserialize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::http::{Response, header::CONTENT_TYPE};
use wry::{PageLoadEvent, WebViewBuilder, WebViewBuilderExtUnix};

enum UserEvent {
    Output(String),
    Exit,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Input { data: String },
}

pub fn run(shell: &str) -> Result<(), Error> {
    let (pty, reader) = Pty::spawn(shell, 80, 24)?;
    let pty = RefCell::new(pty);
    let reader = RefCell::new(Some(reader));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new().with_title("rt").build(&event_loop)?;

    let webview = WebViewBuilder::new()
        .with_custom_protocol("rt".into(), |_, req| serve(req.uri().path()))
        .with_url("rt://app/")
        // Output read before the page is ready would be lost, so the pty is
        // only drained once the page has loaded; the kernel buffers until then.
        .with_on_page_load_handler(move |event, _| {
            if let PageLoadEvent::Finished = event
                && let Some(reader) = reader.borrow_mut().take()
            {
                let proxy = proxy.clone();
                thread::spawn(move || pump(reader, proxy));
            }
        })
        .with_ipc_handler(move |req| match serde_json::from_str(req.body()) {
            Ok(Message::Input { data }) => {
                let _ = pty.borrow_mut().write(data.as_bytes());
            }
            Err(e) => eprintln!("rt: bad ipc message: {e}"),
        })
        .build_gtk(window.default_vbox().ok_or("no GTK container")?)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Output(data)) => {
                let event = serde_json::json!({ "type": "raw", "data": data });
                let _ = webview.evaluate_script(&format!("rt.event({event})"));
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

fn pump(mut reader: Box<dyn Read + Send>, proxy: EventLoopProxy<UserEvent>) {
    let mut buf = [0; 64 * 1024];
    let mut pending = Vec::new();
    while let Ok(n @ 1..) = reader.read(&mut buf) {
        pending.extend_from_slice(&buf[..n]);
        let text = decode_utf8(&mut pending);
        if !text.is_empty() && proxy.send_event(UserEvent::Output(text)).is_err() {
            return;
        }
    }
    let _ = proxy.send_event(UserEvent::Exit);
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
        .body(Cow::Borrowed(body.as_bytes()))
        .unwrap()
}
