//! Latency tracing, built only with the `trace` feature: `trace!` then
//! appends the wall clock in nanoseconds and an event to the file named by
//! `RATERM_TRACE`, which `extra/bench/trace.py` lines up with the bench's
//! samples. Without the feature, `trace!` expands to nothing.

#[cfg(feature = "trace")]
pub fn write(what: std::fmt::Arguments) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    static OUT: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let out = OUT.get_or_init(|| {
        let path = std::env::var_os("RATERM_TRACE")?;
        std::fs::File::create(path).ok().map(Mutex::new)
    });
    if let Some(out) = out {
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let _ = writeln!(out.lock().unwrap(), "{ns} {what}");
    }
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        #[cfg(feature = "trace")]
        $crate::trace::write(format_args!($($arg)*));
    }};
}
