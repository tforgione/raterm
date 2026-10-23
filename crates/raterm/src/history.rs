//! The scrollback, kept whole by the core, unbounded by default.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;

use crate::rich::{Kind, Rendered};
use crate::term::{Line, Run, Shown};

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    /// A logical line: the rows of a soft-wrapped line are joined.
    Line(Line),
    Block(Rendered),
}

#[derive(Default)]
pub struct History {
    /// Number of the oldest entry kept; numbers are never reused.
    first: u64,
    entries: VecDeque<Entry>,
    /// Entries kept at most; unbounded when `None`.
    pub cap: Option<usize>,
    /// Entry of the last block with each id.
    ids: HashMap<String, u64>,
}

impl History {
    pub fn first(&self) -> u64 {
        self.first
    }

    /// Number of the next entry, one past the last one.
    pub fn next(&self) -> u64 {
        self.first + self.entries.len() as u64
    }

    /// Appends a row that left the screen; it continues the last line when
    /// that one soft-wraps. Returns the number of its entry.
    pub fn push_row(&mut self, mut row: Line) -> u64 {
        if let Some(Entry::Line(last)) = self.entries.back_mut()
            && last.wrap
        {
            let at = last.cells();
            last.images
                .extend(row.images.iter().map(|&s| Shown { at: s.at + at, ..s }));
            last.runs.extend(row.runs);
            last.runs = fitted(std::mem::take(&mut last.runs));
            last.wrap = row.wrap;
            return self.next() - 1;
        }
        // A run takes 128 bytes, and a line has few: the room a growing Vec
        // keeps would double the memory of the scrollback. A Vec of the right
        // size, rather than `shrink_to_fit`, whose reallocations in place left
        // holes no line reuses (617 MiB against 376 for 1,000,000 lines).
        row.runs = fitted(row.runs);
        self.push(Entry::Line(row))
    }

    pub fn push(&mut self, entry: Entry) -> u64 {
        if let Entry::Block(Rendered { id: Some(id), .. }) = &entry {
            self.ids.insert(id.clone(), self.next());
        }
        self.entries.push_back(entry);
        while self.cap.is_some_and(|cap| self.entries.len() > cap) {
            self.entries.pop_front();
            self.first += 1;
        }
        self.next() - 1
    }

    /// The entry of the block with this id, if it is still kept.
    pub fn block(&self, id: &str) -> Option<u64> {
        self.ids.get(id).copied().filter(|&n| n >= self.first)
    }

    pub fn replace(&mut self, n: u64, entry: Entry) {
        if let Some(i) = n.checked_sub(self.first)
            && let Some(old) = self.entries.get_mut(i as usize)
        {
            *old = entry;
        }
    }

    pub fn get(&self, n: u64) -> Option<&Entry> {
        let i = n.checked_sub(self.first)?;
        self.entries.get(usize::try_from(i).ok()?)
    }
}

/// `runs` in a Vec of their size, newly allocated (see `push_row`).
fn fitted(mut runs: Vec<Run>) -> Vec<Run> {
    let mut fit = Vec::with_capacity(runs.len());
    fit.append(&mut runs);
    fit
}

/// The text a block is searched in: the source of Markdown, the text of HTML
/// without its tags; nothing for images and media.
pub fn block_text(block: &Rendered) -> Option<String> {
    match block.kind {
        Kind::Md => block.text.clone(),
        Kind::Html => {
            let mut text = String::new();
            let mut in_tag = false;
            for c in block.content.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    c if !in_tag => text.push(c),
                    _ => {}
                }
            }
            Some(
                text.replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&#39;", "'")
                    .replace("&nbsp;", " ")
                    .replace("&amp;", "&"),
            )
        }
        _ => None,
    }
}

/// Positions of `needle` in `text`, ignoring case, in UTF-16 code units of
/// the lowercased text, as WebKit counts them in blocks.
pub fn matches(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let text = text.to_lowercase();
    let units = |s: &str| s.encode_utf16().count();
    text.match_indices(needle)
        .map(|(i, m)| {
            let start = units(&text[..i]);
            (start, start + units(m))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Run;

    fn line(text: &str, wrap: bool) -> Line {
        Line {
            runs: vec![Run {
                text: text.into(),
                ..Default::default()
            }],
            wrap,
            images: Vec::new(),
        }
    }

    fn text(history: &History, n: u64) -> String {
        match history.get(n) {
            Some(Entry::Line(line)) => line.runs.iter().map(|r| r.text.as_str()).collect(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn soft_wrapped_rows_join_one_entry() {
        let mut history = History::default();
        assert_eq!(history.push_row(line("ab", true)), 0);
        assert_eq!(history.push_row(line("cd", false)), 0);
        assert_eq!(history.push_row(line("ef", false)), 1);
        assert_eq!(text(&history, 0), "abcd");
        assert_eq!(history.next(), 2);
    }

    #[test]
    fn cap_drops_the_oldest_without_renumbering() {
        let mut history = History {
            cap: Some(2),
            ..Default::default()
        };
        for t in ["a", "b", "c"] {
            history.push_row(line(t, false));
        }
        assert_eq!((history.first(), history.next()), (1, 3));
        assert_eq!(history.get(0), None);
        assert_eq!(text(&history, 2), "c");
    }

    #[test]
    fn blocks_are_found_by_id_while_kept() {
        let mut history = History {
            cap: Some(1),
            ..Default::default()
        };
        let block = Rendered {
            kind: crate::rich::Kind::Html,
            content: "<b>".into(),
            id: Some("p".into()),
            w: None,
            h: None,
            text: None,
        };
        let n = history.push(Entry::Block(block));
        assert_eq!(history.block("p"), Some(n));
        history.push_row(line("x", false));
        assert_eq!(history.block("p"), None);
    }

    #[test]
    fn matches_count_utf16_units_ignoring_case() {
        assert_eq!(matches("🦀 Rust rust", "rust"), [(3, 7), (8, 12)]);
    }
}
