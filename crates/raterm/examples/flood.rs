//! Times the core on a flood: `cat`s each file in a pty and pumps it through
//! the terminal and the JSON encoding, as the window would, without one.
//!
//!     cargo run --release --example flood -- [--size COLSxROWS] FILE...

use std::time::{Duration, Instant};

use raterm::config::Config;
use raterm::session::Session;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let (mut cols, mut rows) = (150, 45);
    if args.first().is_some_and(|a| a == "--size") {
        let size = args.drain(..2).nth(1).expect("--size COLSxROWS");
        let (c, r) = size.split_once('x').expect("--size COLSxROWS");
        (cols, rows) = (c.parse().unwrap(), r.parse().unwrap());
    }
    let config = Config {
        scrollback: Some(10_000),
        ..Config::default()
    };
    for file in &args {
        let command = ["cat".to_owned(), file.clone()];
        let (session, reader) = Session::spawn(&config, &command, cols, rows).unwrap();
        let (mut batches, mut json, mut encode) = (0, 0, Duration::ZERO);
        let start = Instant::now();
        session.pump(reader, |events| {
            let t = Instant::now();
            json += serde_json::to_string(&events).unwrap().len();
            encode += t.elapsed();
            batches += 1;
        });
        println!(
            "{file}: {:.3} s, {batches} batches, {:.1} MiB of JSON encoded in {:.3} s",
            start.elapsed().as_secs_f64(),
            json as f64 / (1 << 20) as f64,
            encode.as_secs_f64(),
        );
    }
}
