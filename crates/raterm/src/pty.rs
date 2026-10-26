use std::io::{Read, Write};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Spawns `command` (program and arguments) in a new pty of `cols × rows`
    /// cells, `width × height` pixels, and returns it with a reader on its
    /// output.
    pub fn spawn(
        command: &[String],
        cols: u16,
        rows: u16,
        width: u16,
        height: u16,
    ) -> Result<(Pty, Box<dyn Read + Send>), Error> {
        let pair = native_pty_system().openpty(size(cols, rows, width, height))?;
        let mut cmd = CommandBuilder::from_argv(command.iter().map(Into::into).collect());
        cmd.env("TERM", "xterm-256color");
        cmd.env("TERM_PROGRAM", "raterm");
        // Without it, tmux turns 24-bit colors into 256 ones.
        cmd.env("COLORTERM", "truecolor");
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
    #[cfg(target_os = "linux")]
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child.process_id()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Current directory of the program.
    #[cfg(target_os = "macos")]
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let pid = self.child.process_id()?;
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
        // SAFETY: `info` is as large as told, and zeroed.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        // SAFETY: filled by the kernel.
        let info = unsafe { info.assume_init() };
        // libc splits the path in chunks, for old compilers.
        let path = info.pvi_cdir.vip_path.as_flattened();
        let bytes: Vec<u8> = path
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some(std::ffi::OsStr::from_bytes(&bytes).into())
    }

    /// Resizes to `cols × rows` cells, `width × height` pixels in all.
    pub fn resize(&self, cols: u16, rows: u16, width: u16, height: u16) -> Result<(), Error> {
        Ok(self.master.resize(size(cols, rows, width, height))?)
    }
}

fn size(cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width,
        pixel_height,
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
        let (mut pty, mut reader) = Pty::spawn(&["/bin/sh".into()], 80, 24, 0, 0).unwrap();
        pty.write(b"echo rt-$((40+2))\nexit\n").unwrap();
        let mut out = Vec::new();
        // The read ends with EIO on Linux once the child is gone.
        let _ = reader.read_to_end(&mut out);
        assert!(String::from_utf8_lossy(&out).contains("rt-42"));
    }

    #[test]
    fn cwd_follows_the_shell() {
        let (mut pty, mut reader) = Pty::spawn(&["/bin/sh".into()], 80, 24, 0, 0).unwrap();
        pty.write(b"cd /usr/bin && echo rt-$((40+2))\n").unwrap();
        let mut out = Vec::new();
        let mut buf = [0; 1024];
        while !String::from_utf8_lossy(&out).contains("rt-42") {
            let n = reader.read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        let cwd = pty.cwd().unwrap().canonicalize().unwrap();
        assert_eq!(
            cwd,
            std::path::Path::new("/usr/bin").canonicalize().unwrap()
        );
    }
}
