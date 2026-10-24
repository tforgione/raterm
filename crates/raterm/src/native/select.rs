//! The selection of the native view, over the flow: positions are an entry of
//! the history, or `next + y` for row `y` of the screen (as in
//! `Terminal::search`), and a cell of that line.

use raterm::history::Entry;
use raterm::term::{Line, Terminal};

use super::runs::{cells, units};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Pos {
    pub(super) at: u64,
    pub(super) cell: usize,
}

/// What a drag extends: cells, words (double click) or lines (triple click).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Unit {
    Cell,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Selection {
    pub(super) anchor: Pos,
    pub(super) head: Pos,
    pub(super) unit: Unit,
    /// A block (Ctrl+drag), with the columns of the view: the same columns
    /// of each row of the view, the cells of the anchor and the head included.
    pub(super) block: Option<usize>,
}

/// A line of the flow: an entry of the history or a row of the screen.
enum Item<'a> {
    Line(&'a Line),
    /// The text a block copies as (its Markdown source).
    Block(Option<&'a str>),
}

fn item(term: &Terminal, at: u64) -> Option<Item<'_>> {
    let next = term.history.next();
    if at >= next {
        return term.screen().get((at - next) as usize).map(Item::Line);
    }
    match term.history.get(at)? {
        Entry::Line(line) => Some(Item::Line(line)),
        Entry::Block(block) => Some(Item::Block(block.text.as_deref())),
    }
}

/// What each cell of a line holds (a char, with its zero-width chars), with
/// the cell it starts at.
pub(super) fn chars(line: &Line) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut col = 0;
    for run in &line.runs {
        out.extend(units(run).into_iter().map(|(x, text)| (col + x, text)));
        col += cells(run);
    }
    out
}

fn first(text: &str) -> char {
    text.chars().next().unwrap_or(' ')
}

/// Separators of words on a double click, as alacritty's `semantic_escape_chars`.
fn word_char(c: char) -> bool {
    !c.is_whitespace() && !",│`|:\"'()[]{}<>".contains(c)
}

impl Selection {
    /// Start and end, in order, extended to the unit.
    pub(super) fn bounds(&self, term: &Terminal) -> (Pos, Pos) {
        let (mut a, mut b) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        match self.unit {
            Unit::Cell => {}
            Unit::Line => {
                a.cell = 0;
                b.cell = usize::MAX;
            }
            Unit::Word => {
                if let Some(Item::Line(line)) = item(term, a.at) {
                    let cs = chars(line);
                    let i = cs.iter().rposition(|&(x, _)| x <= a.cell);
                    if let Some(i) = i.filter(|&i| word_char(first(cs[i].1))) {
                        let start = cs[..i].iter().rposition(|&(_, c)| !word_char(first(c)));
                        a.cell = start.map_or(0, |s| cs[s + 1].0);
                    }
                }
                if let Some(Item::Line(line)) = item(term, b.at) {
                    let cs = chars(line);
                    if let Some(i) = cs.iter().rposition(|&(x, _)| x <= b.cell)
                        && word_char(first(cs[i].1))
                    {
                        let end = cs[i..].iter().position(|&(_, c)| !word_char(first(c)));
                        b.cell = end.map_or(usize::MAX, |e| cs[i + e].0);
                    } else {
                        b.cell += 1;
                    }
                }
            }
        }
        (a, b)
    }

    /// The rows of the view a block spans, as (entry, row of the entry), and
    /// its columns.
    fn rect(&self, cols: usize) -> ((u64, usize), (u64, usize), usize, usize) {
        let row = |p: Pos| (p.at, p.cell / cols);
        let (a, h) = (row(self.anchor), row(self.head));
        let (ca, ch) = (self.anchor.cell % cols, self.head.cell % cols);
        (a.min(h), a.max(h), ca.min(ch), ca.max(ch) + 1)
    }

    /// The selected cells of the row of the view that starts at cell `start`
    /// of line `at`, as a range.
    pub(super) fn cells_of(
        &self,
        term: &Terminal,
        at: u64,
        start: usize,
    ) -> Option<(usize, usize)> {
        if let Some(cols) = self.block {
            let (top, bottom, c1, c2) = self.rect(cols);
            let row = (at, start / cols);
            return (top <= row && row <= bottom).then_some((start + c1, start + c2));
        }
        let (a, b) = self.bounds(term);
        if at < a.at || at > b.at {
            return None;
        }
        let start = if at == a.at { a.cell } else { 0 };
        let end = if at == b.at { b.cell } else { usize::MAX };
        (start < end).then_some((start, end))
    }

    /// The selected text: lines end with a newline, except a soft-wrapped row
    /// of the screen, which goes on in the next one; trailing blanks go.
    pub(super) fn text(&self, term: &Terminal) -> String {
        if let Some(cols) = self.block {
            return self.block_text(term, cols);
        }
        let (a, b) = self.bounds(term);
        let mut out = String::new();
        for at in a.at..=b.at {
            let Some(item) = item(term, at) else { continue };
            let wrap = match item {
                Item::Block(text) => {
                    out += text.unwrap_or("");
                    false
                }
                Item::Line(line) => {
                    let (s, e) = self.cells_of(term, at, 0).unwrap_or((0, 0));
                    let text: String = chars(line)
                        .into_iter()
                        .filter(|&(x, _)| x >= s && x < e)
                        .map(|(_, c)| c)
                        .collect();
                    out += text.trim_end();
                    line.wrap
                }
            };
            if at != b.at && !wrap {
                out.push('\n');
            }
        }
        out
    }

    /// A line per row of the view in the block, without its trailing blanks.
    fn block_text(&self, term: &Terminal, cols: usize) -> String {
        let (top, bottom, ..) = self.rect(cols);
        let mut rows = Vec::new();
        for at in top.0..=bottom.0 {
            match item(term, at) {
                Some(Item::Line(line)) => {
                    let len: usize = line.runs.iter().map(cells).sum();
                    for start in (0..len.max(1).div_ceil(cols)).map(|r| r * cols) {
                        if let Some((s, e)) = self.cells_of(term, at, start) {
                            let text: String = chars(line)
                                .into_iter()
                                .filter(|&(x, _)| x >= s && x < e)
                                .map(|(_, c)| c)
                                .collect();
                            rows.push(text.trim_end().to_owned());
                        }
                    }
                }
                Some(Item::Block(text)) => rows.push(text.unwrap_or("").to_owned()),
                None => {}
            }
        }
        rows.join("\n")
    }

    /// A click without a drag: a block starts empty, as a single cell would
    /// leave no room to tell a Ctrl+click on a link apart.
    pub(super) fn is_empty(&self) -> bool {
        self.unit == Unit::Cell && self.anchor == self.head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(term: &Terminal, anchor: (u64, usize), head: (u64, usize), unit: Unit) -> String {
        let pos = |(at, cell)| Pos { at, cell };
        Selection {
            anchor: pos(anchor),
            head: pos(head),
            unit,
            block: None,
        }
        .text(term)
    }

    #[test]
    fn selection_goes_from_history_to_screen() {
        let mut term = Terminal::new(20, 2);
        term.advance(b"hello world\r\nfoo bar\r\nbaz\r\nqux");
        term.refresh();
        // Entries 0 "hello world" and 1 "foo bar"; screen rows 2 "baz", 3 "qux".
        assert_eq!(select(&term, (0, 6), (1, 3), Unit::Cell), "world\nfoo");
        assert_eq!(select(&term, (1, 4), (3, 2), Unit::Cell), "bar\nbaz\nqu");
        // Backwards is the same.
        assert_eq!(select(&term, (1, 3), (0, 6), Unit::Cell), "world\nfoo");
    }

    #[test]
    fn words_and_lines() {
        let mut term = Terminal::new(30, 2);
        term.advance(b"ls (/usr/bin) done\r\n$ ");
        term.refresh();
        // No history: the line is row 0 of the screen.
        assert_eq!(select(&term, (0, 6), (0, 6), Unit::Word), "/usr/bin");
        assert_eq!(select(&term, (0, 0), (0, 0), Unit::Word), "ls");
        assert_eq!(
            select(&term, (0, 3), (0, 3), Unit::Line),
            "ls (/usr/bin) done"
        );
    }

    #[test]
    fn soft_wrapped_rows_copy_as_one_line() {
        let mut term = Terminal::new(4, 3);
        term.advance(b"abcdef\r\ngh");
        term.refresh();
        assert_eq!(select(&term, (0, 0), (2, 2), Unit::Cell), "abcdef\ngh");
    }

    fn select_block(
        term: &Terminal,
        anchor: (u64, usize),
        head: (u64, usize),
        cols: usize,
    ) -> String {
        let pos = |(at, cell)| Pos { at, cell };
        Selection {
            anchor: pos(anchor),
            head: pos(head),
            unit: Unit::Cell,
            block: Some(cols),
        }
        .text(term)
    }

    #[test]
    fn a_block_takes_the_same_columns_of_each_row() {
        let mut term = Terminal::new(12, 3);
        term.advance(b"left | right\r\nab   | cd\r\nxyz  | w");
        term.refresh();
        // Columns 7 to 11 of the three rows, the panel on the right.
        assert_eq!(select_block(&term, (0, 7), (2, 11), 12), "right\ncd\nw");
        // Dragged up and left, the same block.
        assert_eq!(select_block(&term, (2, 11), (0, 7), 12), "right\ncd\nw");
        assert_eq!(select_block(&term, (0, 0), (2, 3), 12), "left\nab\nxyz");
    }

    #[test]
    fn a_block_cuts_a_wrapped_entry_into_its_rows_of_the_view() {
        let mut term = Terminal::new(4, 2);
        // "abcdefgh" soft-wraps: one entry of the history, two rows of the view.
        term.advance(b"abcdefgh\r\nij\r\n\r\n");
        term.refresh();
        // Columns 1 and 2 of row 0 (cells 1..3) to row 1 of the entry (cells 5..7).
        assert_eq!(select_block(&term, (0, 1), (0, 6), 4), "bc\nfg");
        assert_eq!(select_block(&term, (0, 5), (1, 6), 4), "fg\nj");
    }

    #[test]
    fn blocks_copy_as_their_text() {
        let mut term = Terminal::new(20, 2);
        // "IyB0" is base64 for "# t".
        term.advance(b"a\r\n\x1b]7117;md;IyB0\x07b");
        term.refresh();
        let n = term.history.next();
        assert_eq!(select(&term, (0, 0), (n, 1), Unit::Cell), "a\n# t\nb");
    }
}
