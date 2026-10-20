use std::io::{Read, Write};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Pty {
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Spawns `program` in a new pty and returns it with a reader on its output.
    pub fn spawn(
        program: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(Pty, Box<dyn Read + Send>), Error> {
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(program);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        // Dropping the slave lets reads on the master fail once the child exits.
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        Ok((Pty { writer, child }, reader))
    }

    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Splits `buf` into its longest valid UTF-8 prefix and an incomplete trailing
/// sequence to carry over to the next read. Invalid bytes are replaced.
pub fn decode_utf8(buf: &mut Vec<u8>) -> String {
    let keep = match std::str::from_utf8(buf) {
        Ok(_) => 0,
        Err(e) if e.error_len().is_none() => buf.len() - e.valid_up_to(),
        Err(_) => 0,
    };
    let tail = buf.split_off(buf.len() - keep);
    let text = String::from_utf8_lossy(buf).into_owned();
    *buf = tail;
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_echoes_through_pty() {
        let (mut pty, mut reader) = Pty::spawn("/bin/sh", 80, 24).unwrap();
        pty.write(b"echo rt-$((40+2))\nexit\n").unwrap();
        let mut out = Vec::new();
        // The read ends with EIO on Linux once the child is gone.
        let _ = reader.read_to_end(&mut out);
        assert!(String::from_utf8_lossy(&out).contains("rt-42"));
    }

    #[test]
    fn utf8_split_across_reads() {
        let mut buf = "é".as_bytes()[..1].to_vec();
        assert_eq!(decode_utf8(&mut buf), "");
        buf.extend_from_slice(&"é!".as_bytes()[1..]);
        assert_eq!(decode_utf8(&mut buf), "é!");
        assert!(buf.is_empty());
    }
}
