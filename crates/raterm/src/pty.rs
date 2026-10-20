use std::io::{Read, Write};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Spawns `command` (program and arguments) in a new pty and returns it
    /// with a reader on its output.
    pub fn spawn(
        command: &[String],
        cols: u16,
        rows: u16,
    ) -> Result<(Pty, Box<dyn Read + Send>), Error> {
        let pair = native_pty_system().openpty(size(cols, rows))?;
        let mut cmd = CommandBuilder::from_argv(command.iter().map(Into::into).collect());
        cmd.env("TERM", "xterm-256color");
        cmd.env("TERM_PROGRAM", "raterm");
        cmd.env("RATERM", "1");
        let child = pair.slave.spawn_command(cmd)?;
        // Dropping the slave lets reads on the master fail once the child exits.
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        Ok((
            Pty {
                master: pair.master,
                writer,
                child,
            },
            reader,
        ))
    }

    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    /// Current directory of the program, read from procfs.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child.process_id()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        Ok(self.master.resize(size(cols, rows))?)
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_echoes_through_pty() {
        let (mut pty, mut reader) = Pty::spawn(&["/bin/sh".into()], 80, 24).unwrap();
        pty.write(b"echo rt-$((40+2))\nexit\n").unwrap();
        let mut out = Vec::new();
        // The read ends with EIO on Linux once the child is gone.
        let _ = reader.read_to_end(&mut out);
        assert!(String::from_utf8_lossy(&out).contains("rt-42"));
    }
}
