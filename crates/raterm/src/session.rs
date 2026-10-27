use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::config::{Config, CursorStyle};
use crate::pty::{Error, Output, Pty};
use crate::term::{CursorShape, Terminal, Update};

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
    /// Runs `command` (program and arguments) in a new terminal of
    /// `cols × rows` cells of `cell` pixels.
    pub fn spawn(
        config: &Config,
        command: &[String],
        cols: u16,
        rows: u16,
        cell: (f64, f64),
    ) -> Result<(Arc<Session>, Output), Error> {
        let mut term = Terminal::new(cols, rows);
        term.resize(cols, rows, cell);
        let (width, height) = term.text_area();
        let (pty, output) = Pty::spawn(command, cols, rows, width, height)?;
        term.bold_is_bright = config.bold_is_bright;
        term.history.cap = config.scrollback;
        let shape = match config.cursor.style {
            CursorStyle::Block => CursorShape::Block,
            CursorStyle::Bar => CursorShape::Beam,
            CursorStyle::Underline => CursorShape::Underline,
        };
        term.set_default_cursor(shape, config.cursor.blink);
        let session = Session {
            pty: Mutex::new(pty),
            term: Mutex::new(term),
        };
        Ok((Arc::new(session), output))
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
        let mut term = self.term.lock().unwrap();
        term.refresh();
        f(&term)
    }

    /// Resizes to `cols × rows` cells of `cell` pixels.
    pub fn resize(&self, cols: u16, rows: u16, cell: (f64, f64), emit: impl FnOnce(Update)) {
        let mut term = self.term.lock().unwrap();
        let update = term.resize(cols, rows, cell);
        let (width, height) = term.text_area();
        if let Err(e) = self.pty.lock().unwrap().resize(cols, rows, width, height) {
            eprintln!("raterm: pty resize failed: {e}");
        }
        self.dispatch(update, emit);
    }

    /// Reads the pty until the shell exits.
    ///
    /// The pty hands out a flood 4 KiB at a time; reading on a thread of its
    /// own lets this one take all that is pending, up to `BATCH` bytes, so
    /// that the screen is rendered once per batch, not per read.
    pub fn pump(&self, output: Output, mut emit: impl FnMut(Update)) {
        const READ: usize = 16 * 1024;
        const BATCH: usize = 64 * 1024;
        let (mut reader, wait) = output.split();
        let (tx, rx) = mpsc::sync_channel(BATCH / READ);
        // An empty read tells that the shell exited.
        let exit = tx.clone();
        thread::spawn(move || {
            wait();
            let _ = exit.send(Vec::new());
        });
        thread::spawn(move || {
            let mut buf = vec![0; READ];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                crate::trace!("read {n}");
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        while let Ok(mut batch) = rx.recv() {
            let mut exited = batch.is_empty();
            while !exited
                && batch.len() + READ <= BATCH
                && let Ok(more) = rx.try_recv()
            {
                exited = more.is_empty();
                batch.extend(more);
            }
            if !batch.is_empty() {
                let mut term = self.term.lock().unwrap();
                crate::trace!("advance {}", batch.len());
                let update = term.advance(&batch);
                crate::trace!("advanced");
                self.dispatch(update, &mut emit);
            }
            if exited {
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn pump_ends_with_the_shell_not_with_the_pty() {
        // Like `wl-copy`: outlives the shell, with the terminal as stderr.
        let command = [
            "/bin/sh",
            "-c",
            "(trap '' HUP; exec sleep 5 </dev/null >/dev/null) & exit",
        ]
        .map(String::from);
        let (session, output) =
            Session::spawn(&Config::default(), &command, 80, 24, (0.0, 0.0)).unwrap();
        let start = Instant::now();
        session.pump(output, |_| ());
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
