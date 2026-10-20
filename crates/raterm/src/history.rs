//! The scrollback, kept whole by the core: the page only holds a window of
//! it, which it asks for as the user scrolls.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;

use crate::rich::Rendered;
use crate::term::Line;

/// Entries the page holds at most, and the most the core sends at once: past
/// about 10,000 lines in the DOM, every frame slows down.
pub const WINDOW: usize = 5000;

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    /// A logical line: the rows of a soft-wrapped line are joined.
    Line(Line),
    Block(Rendered),
}

/// An entry with its number, as sent to the page.
#[derive(Serialize, Debug, PartialEq)]
pub struct Numbered {
    pub entry: u64,
    #[serde(flatten)]
    pub item: Entry,
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
    pub fn push_row(&mut self, row: Line) -> u64 {
        if let Some(Entry::Line(last)) = self.entries.back_mut()
            && last.wrap
        {
            last.runs.extend(row.runs);
            last.wrap = row.wrap;
            return self.next() - 1;
        }
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

    /// Entries `from..from + count` that are kept, at most `WINDOW`.
    pub fn slice(&self, from: u64, count: usize) -> Vec<Numbered> {
        let from = from.max(self.first);
        (from..self.next())
            .take(count.min(WINDOW))
            .map(|n| Numbered {
                entry: n,
                item: self.get(n).unwrap().clone(),
            })
            .collect()
    }
}

/// Positions of `needle` in `text`, ignoring case, in UTF-16 code units of
/// the lowercased text, as the page counts them.
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
                style: Default::default(),
            }],
            wrap,
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
        let slice: Vec<u64> = history.slice(0, 10).iter().map(|e| e.entry).collect();
        assert_eq!(slice, [1, 2]);
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
