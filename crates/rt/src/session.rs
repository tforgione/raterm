use std::io::Read;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::pty::{Error, Pty};
use crate::term::{Event, Terminal, Update};

/// A shell running in a pty, wired to the terminal state.
///
/// Events are emitted while the terminal is locked, so batches coming from the
/// reader thread and from resizes reach the view in the order they were computed.
pub struct Session {
    pty: Mutex<Pty>,
    term: Mutex<Terminal>,
}

impl Session {
    pub fn spawn(
        config: &Config,
        cols: u16,
        rows: u16,
    ) -> Result<(Arc<Session>, Box<dyn Read + Send>), Error> {
        let (pty, reader) = Pty::spawn(&config.shell(), cols, rows)?;
        let mut term = Terminal::new(cols, rows);
        term.bold_is_bright = config.bold_is_bright;
        let session = Session {
            pty: Mutex::new(pty),
            term: Mutex::new(term),
        };
        Ok((Arc::new(session), reader))
    }

    pub fn input(&self, data: &[u8]) {
        if let Err(e) = self.pty.lock().unwrap().write(data) {
            eprintln!("rt: pty write failed: {e}");
        }
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.pty.lock().unwrap().cwd()
    }

    pub fn paste(&self, text: &str) {
        let data = self.term.lock().unwrap().paste(text);
        self.input(data.as_bytes());
    }

    pub fn resize(&self, cols: u16, rows: u16, emit: impl FnOnce(Vec<Event>)) {
        let mut term = self.term.lock().unwrap();
        let update = term.resize(cols, rows);
        if let Err(e) = self.pty.lock().unwrap().resize(cols, rows) {
            eprintln!("rt: pty resize failed: {e}");
        }
        self.dispatch(update, emit);
    }

    /// Reads the pty until the shell exits.
    pub fn pump(&self, mut reader: Box<dyn Read + Send>, mut emit: impl FnMut(Vec<Event>)) {
        let mut buf = vec![0; 64 * 1024];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            let mut term = self.term.lock().unwrap();
            let update = term.advance(&buf[..n]);
            self.dispatch(update, &mut emit);
        }
    }

    fn dispatch(&self, update: Update, emit: impl FnOnce(Vec<Event>)) {
        if !update.reply.is_empty() {
            self.input(update.reply.as_bytes());
        }
        if !update.events.is_empty() {
            emit(update.events);
        }
    }
}
