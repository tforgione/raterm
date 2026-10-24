//! What the view asks of the system: links, new windows, the clipboard.

use std::thread;

/// Which clipboard.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Which {
    Clipboard,
    /// The primary selection, where there is one (Linux); on macOS, the
    /// backend ignores it.
    Primary,
}

/// The clipboards of a backend: GTK's on Linux, `NSPasteboard` on macOS.
pub(super) trait Clipboard {
    fn set(&self, which: Which, text: &str);
    /// Reads the text of a clipboard, maybe later, and gives it to `then`.
    fn paste(&self, which: Which, then: impl FnOnce(String) + 'static);
}

/// The desktop's handler of links.
#[cfg(not(target_os = "macos"))]
const OPENER: &str = "xdg-open";
#[cfg(target_os = "macos")]
const OPENER: &str = "open";

/// Opens a link in the desktop's handler.
pub(super) fn open_url(url: &str) {
    // Other schemes (file:, custom app handlers) would let terminal output
    // launch arbitrary local handlers on a click.
    if !["http://", "https://", "mailto:"]
        .iter()
        .any(|s| url.starts_with(s))
    {
        return eprintln!("raterm: refusing to open {url:?}");
    }
    match std::process::Command::new(OPENER).arg(url).spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("raterm: {OPENER} failed: {e}"),
    }
}

/// Opens another window, in the same directory as this one's shell.
pub(super) fn new_window(cwd: Option<std::path::PathBuf>) {
    let mut command = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe),
        Err(e) => return eprintln!("raterm: cannot find own executable: {e}"),
    };
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    match command.spawn() {
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(e) => eprintln!("raterm: cannot open a new window: {e}"),
    }
}
