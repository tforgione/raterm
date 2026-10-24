//! The cells the runs of a line take.

use raterm::term::{Attrs, Run};

/// Cells a run takes.
pub(super) fn cells(run: &Run) -> usize {
    match run.cluster {
        0 => run.text.chars().count() * wide(run),
        n => n as usize,
    }
}

/// Cells each char of a run takes.
pub(super) fn wide(run: &Run) -> usize {
    if run.style.attrs.has(Attrs::WIDE) {
        2
    } else {
        1
    }
}

/// What each cell of a run holds, with the cell it starts at in the run: a
/// char, or the whole run when it is a unit (see `Run::cluster`).
pub(super) fn units(run: &Run) -> Vec<(usize, &str)> {
    if run.cluster > 0 {
        return vec![(0, run.text.as_str())];
    }
    let wide = wide(run);
    run.text
        .char_indices()
        .enumerate()
        .map(|(i, (at, c))| (i * wide, &run.text[at..at + c.len_utf8()]))
        .collect()
}

/// A regional indicator: two of them are a flag.
pub(super) fn regional(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|c| ('\u{1f1e6}'..='\u{1f1ff}').contains(&c))
}
