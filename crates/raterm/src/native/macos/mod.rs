//! The view on macOS (see Spec-macOS in the wiki): a `tao` window, the grid
//! drawn with Core Graphics and Core Text, rich blocks in `WKWebView`s.

use raterm::config::Config;
use raterm::pty::Error;

pub fn run(_config: Config, _command: &[String], _title: String) -> Result<(), Error> {
    todo!("macOS: the view")
}
