//! The view (see Spec-View and Spec-macOS in the wiki): a backend per
//! platform, GTK on Linux and AppKit on macOS.

mod keys;
mod runs;
mod search;
mod select;
mod theme;

#[cfg(target_os = "linux")]
mod gtk;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use gtk::run;
#[cfg(target_os = "macos")]
pub use macos::run;
