//! Replays recorded pty sessions (`tests/data/*.in`, see `record.sh`) and
//! compares the final screen and emitted events with `*.out`.
//! Run with `RT_UPDATE_SNAPSHOTS=1` to rewrite the snapshots.

use std::fs;
use std::path::Path;

use rt::term::Terminal;

fn replay(input: &[u8]) -> String {
    let mut term = Terminal::new(80, 24);
    let mut events = String::new();
    // Fixed chunk size keeps batching, hence events, deterministic.
    for chunk in input.chunks(256) {
        for event in term.advance(chunk).events {
            events += &serde_json::to_string(&event).unwrap();
            events.push('\n');
        }
    }
    format!("{}--- events\n{events}", term.text())
}

#[test]
fn snapshots() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let update = std::env::var_os("RT_UPDATE_SNAPSHOTS").is_some();
    let mut failed = Vec::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "in") {
            continue;
        }
        let actual = replay(&fs::read(&path).unwrap());
        let out = path.with_extension("out");
        if update {
            fs::write(&out, &actual).unwrap();
        } else if fs::read_to_string(&out).ok().as_deref() != Some(actual.as_str()) {
            failed.push(out.display().to_string());
        }
    }
    assert!(failed.is_empty(), "snapshot mismatch: {failed:?}");
}
