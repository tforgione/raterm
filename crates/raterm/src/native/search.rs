//! Search in the native view: the core finds the matches over the whole
//! flow (`Terminal::search`), the view shows them and scrolls to the current
//! one.

use raterm::term::{Event, Line};

use super::select::chars;

/// A range of cells of a line.
type Cells = (usize, usize);

/// The search bar's state while it is open.
#[derive(Default)]
pub(super) struct Search {
    /// Lowercased, as matches ignore case.
    pub(super) needle: Vec<char>,
    /// The current match: its line in the flow, and offsets in the text as
    /// the core counts them (UTF-16 units of the lowercased text).
    pub(super) current: Option<(u64, usize, usize)>,
    /// The block the current match is in, which WebKit searches too, to
    /// highlight its matches.
    pub(super) block: Option<u64>,
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

impl Search {
    pub(super) fn set_query(&mut self, query: &str) {
        self.needle = query.chars().map(lower).collect();
        self.current = None;
    }

    /// Takes the core's answer; false when nothing matched.
    pub(super) fn found(&mut self, event: Event) -> bool {
        self.current = match event {
            Event::Found {
                entry: Some(at),
                start,
                end,
            } => Some((at, start, end)),
            _ => None,
        };
        self.current.is_some()
    }

    /// The cells of the matches in a line, and which of them is the current
    /// one.
    pub(super) fn cells(&self, line: &Line, at: u64) -> (Vec<Cells>, Option<Cells>) {
        let n = self.needle.len();
        if n == 0 {
            return (Vec::new(), None);
        }
        let cs = chars(line);
        let cell_end = |i: usize| cs.get(i).map_or(usize::MAX, |&(x, _)| x);
        let mut found = Vec::new();
        let mut current = None;
        // UTF-16 offset of each char, to know the core's current match.
        let mut units = 0;
        let mut i = 0;
        let mut offsets = Vec::with_capacity(cs.len());
        for &(_, text) in &cs {
            offsets.push(units);
            units += text.chars().map(|c| lower(c).len_utf16()).sum::<usize>();
        }
        // A cell matches by its char: zero-width chars after it are not
        // looked at.
        let first = |text: &str| lower(text.chars().next().unwrap_or(' '));
        while i + n <= cs.len() {
            if (0..n).all(|k| first(cs[i + k].1) == self.needle[k]) {
                let range = (cs[i].0, cell_end(i + n));
                if self
                    .current
                    .is_some_and(|(a, s, _)| a == at && s == offsets[i])
                {
                    current = Some(range);
                } else {
                    found.push(range);
                }
                i += n;
            } else {
                i += 1;
            }
        }
        (found, current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raterm::term::Terminal;

    #[test]
    fn matches_ignore_case_and_know_the_current_one() {
        let mut term = Terminal::new(30, 1);
        term.advance("日 foo Foo".as_bytes());
        let line = &term.screen()[0];
        let mut search = Search::default();
        search.set_query("FOO");
        // The core counts "日 " as 2 UTF-16 units; "日" takes 2 cells.
        search.current = Some((0, 6, 9));
        let (found, current) = search.cells(line, 0);
        assert_eq!(found, [(3, 6)]);
        assert_eq!(current, Some((7, 10)));
        assert_eq!(search.cells(line, 1).1, None);
    }
}
