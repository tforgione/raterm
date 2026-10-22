use std::io::Read;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::config::Config;
use crate::pty::{Error, Pty};
use crate::term::{Terminal, Update};

/// A shell running in a pty, wired to the terminal state.
///
/// Updates are emitted while the terminal is locked, so batches coming from
/// the reader thread and from resizes reach the view in the order they were
/// computed.
pub struct Session {
    pty: Mutex<Pty>,
    term: Mutex<Terminal>,
}

impl Session {
    /// Runs `command` (program and arguments) in a new terminal.
    pub fn spawn(
        config: &Config,
        command: &[String],
        cols: u16,
        rows: u16,
    ) -> Result<(Arc<Session>, Box<dyn Read + Send>), Error> {
        let (pty, reader) = Pty::spawn(command, cols, rows)?;
        let mut term = Terminal::new(cols, rows);
        term.bold_is_bright = config.bold_is_bright;
        term.history.cap = config.scrollback;
        let session = Session {
            pty: Mutex::new(pty),
            term: Mutex::new(term),
        };
        Ok((Arc::new(session), reader))
    }

    pub fn input(&self, data: &[u8]) {
        if let Err(e) = self.pty.lock().unwrap().write(data) {
            eprintln!("raterm: pty write failed: {e}");
        }
    }

    pub fn media(&self) -> crate::media::Media {
        self.term.lock().unwrap().media.clone()
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.pty.lock().unwrap().cwd()
    }

    pub fn paste(&self, text: &str) {
        let data = self.term.lock().unwrap().paste(text);
        self.input(data.as_bytes());
    }

    /// Reads the terminal's state, for a view that draws it itself.
    pub fn read<R>(&self, f: impl FnOnce(&Terminal) -> R) -> R {
        f(&self.term.lock().unwrap())
    }

    pub fn resize(&self, cols: u16, rows: u16, emit: impl FnOnce(Update)) {
        let mut term = self.term.lock().unwrap();
        let update = term.resize(cols, rows);
        if let Err(e) = self.pty.lock().unwrap().resize(cols, rows) {
            eprintln!("raterm: pty resize failed: {e}");
        }
        self.dispatch(update, emit);
    }

    /// Reads the pty until the shell exits.
    ///
    /// The pty hands out a flood 4 KiB at a time; reading on a thread of its
    /// own lets this one take all that is pending, up to `BATCH` bytes, so
    /// that the screen is rendered once per batch, not per read.
    pub fn pump(&self, mut reader: Box<dyn Read + Send>, mut emit: impl FnMut(Update)) {
        const READ: usize = 16 * 1024;
        const BATCH: usize = 64 * 1024;
        let (tx, rx) = mpsc::sync_channel(BATCH / READ);
        thread::spawn(move || {
            let mut buf = vec![0; READ];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        while let Ok(mut batch) = rx.recv() {
            while batch.len() + READ <= BATCH
                && let Ok(more) = rx.try_recv()
            {
                batch.extend(more);
            }
            let mut term = self.term.lock().unwrap();
            let update = term.advance(&batch);
            self.dispatch(update, &mut emit);
        }
    }

    fn dispatch(&self, update: Update, emit: impl FnOnce(Update)) {
        if !update.reply.is_empty() {
            self.input(update.reply.as_bytes());
        }
        if update.changed || update.title.is_some() || update.modes.is_some() {
            emit(update);
        }
    }
}
