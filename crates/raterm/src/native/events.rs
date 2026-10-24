//! The events of the window's loop (`tao`, on both platforms), and the
//! thread that feeds the terminal.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use raterm::session::Session;
use raterm::term::Modes;
use tao::event_loop::EventLoopProxy;

pub(super) enum UserEvent {
    /// New output: lay the flow out again and draw it at the next frame.
    Redraw,
    Title(String),
    Exit,
}

/// Asks the event loop for a redraw, once until it is done.
pub(super) struct Redraw {
    pending: AtomicBool,
    proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl Redraw {
    pub(super) fn new(proxy: EventLoopProxy<UserEvent>) -> Arc<Redraw> {
        Arc::new(Redraw {
            pending: AtomicBool::new(false),
            proxy: Mutex::new(proxy),
        })
    }

    pub(super) fn request(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.proxy.lock().unwrap().send_event(UserEvent::Redraw);
        }
    }

    /// The redraw asked for is being done: the next one is asked again.
    pub(super) fn done(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

/// Feeds the terminal with what the program writes, in a thread: tells the
/// loop the new title, keeps the modes, asks for a redraw after each batch,
/// and tells the loop when the program exits.
pub(super) fn pump(
    session: Arc<Session>,
    reader: Box<dyn Read + Send>,
    modes: Arc<Mutex<Modes>>,
    redraw: Arc<Redraw>,
    proxy: EventLoopProxy<UserEvent>,
) {
    thread::spawn(move || {
        session.pump(reader, |update| {
            if let Some(title) = update.title {
                let _ = proxy.send_event(UserEvent::Title(title));
            }
            if let Some(m) = update.modes {
                *modes.lock().unwrap() = m;
            }
            raterm::trace!("request");
            redraw.request();
        });
        let _ = proxy.send_event(UserEvent::Exit);
    });
}
