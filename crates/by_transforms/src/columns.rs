//! which bytes of each line of the python a transpile writes are text the author wrote, and
//! where in the `.by` that text is
//!
//! the line table says which `.by` line a generated line came from; this says which *columns*.
//! a debugger names a call on a line by the text it covers, and the text of a call the
//! transpiler rewrote is the generated python's: `Point(0.0, 0.0)` is read through the lazy-import
//! holder as `_by_lazy.Point(0.0, 0.0)`, and a comprehension's iterable is wrapped in
//! `_soundness_iter(…)`. with columns it can name the author's call by the author's text, and
//! tell a call the transpiler wrote from one the author did
//!
//! a generated line is described as runs of its bytes, each either
//!
//! - copied: bytes the author wrote, at a line and column of the input, or
//! - a qualifier: bytes the transpiler wrote in front of the author's name to say where that
//!   name lives, which read the same object the name did: the `_by_lazy.` of
//!   `_by_lazy.Point`. an expression is the author's still when only a qualifier is added
//!   to it
//!
//! and every byte in no run was written by the transpiler. columns are byte offsets into their
//! line, as python's own code positions are
//!
//! each step of the transpile describes its output against its input, and the descriptions
//! are composed into one against the `.by`. a step that cannot say where its text came from
//! describes only the lines it left as they were — a line it changed is described as the
//! transpiler's, which loses columns and never claims text the author did not write. what is
//! claimed is checked against the `.by` at the end, so a copied run that is not
//! the author's bytes is never reported as one

use crate::source_map::{Origin, Replacement};

/// where a run of a generated line's bytes came from
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnOrigin {
    /// copied from the input, starting at this 0-based line and byte column of it
    Copied { line: u32, column: u32 },
    /// a qualifier the transpiler wrote in front of the author's name
    Qualifier,
}

/// a run of one generated line's bytes, `start..end` in byte columns
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: u32,
    pub end: u32,
    pub origin: ColumnOrigin,
}

/// the runs of one generated line, ascending and disjoint. what no run covers is the
/// transpiler's
pub type LineColumns = Vec<Segment>;

/// the runs of every line of a text, one entry per line
pub type Columns = Vec<LineColumns>;

/// the lines of `text` as python numbers them: split on `\n`, a final line counted whether or
/// not anything terminates it, and no line after a final `\n`
fn lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.is_empty() || text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// `text` described against itself: every byte copied from where it is
pub(crate) fn identity(text: &str) -> Columns {
    lines(text)
        .iter()
        .enumerate()
        .map(|(index, line)| copied_line(index, line))
        .collect()
}

fn copied_line(index: usize, line: &str) -> LineColumns {
    if line.is_empty() {
        return Vec::new();
    }
    vec![Segment {
        start: 0,
        end: to_u32(line.len()),
        origin: ColumnOrigin::Copied {
            line: to_u32(index),
            column: 0,
        },
    }]
}

/// `text` described against `input`, a text whose lines it keeps one for one: a line equal to
/// the input's line at the same index is copied from it, and any other line is the
/// transpiler's. a text with another number of lines is described as the transpiler's
/// throughout, since nothing says which of its lines is which
///
/// for a step that rewrites text without saying where the result came from, which keeps the
/// lines it did not touch
pub(crate) fn equal_lines(input: &str, text: &str) -> Columns {
    let before = lines(input);
    let after = lines(text);
    if before.len() != after.len() {
        return vec![Vec::new(); after.len()];
    }
    after
        .iter()
        .zip(&before)
        .enumerate()
        .map(|(index, (after, before))| {
            if after == before {
                copied_line(index, after)
            } else {
                Vec::new()
            }
        })
        .collect()
}

/// the 0-based line of `offset` in a text whose lines start at `starts`, and its column
fn position(starts: &[usize], offset: usize) -> (u32, u32) {
    let line = starts
        .partition_point(|&start| start <= offset)
        .saturating_sub(1);
    (to_u32(line), to_u32(offset - starts[line]))
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(index, _)| index + 1))
        .collect()
}

/// builds a text's columns run by run
struct Builder<'a> {
    input_starts: Vec<usize>,
    input: &'a str,
    columns: Columns,
    line: LineColumns,
    column: u32,
}

impl<'a> Builder<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input_starts: line_starts(input),
            input,
            columns: Vec::new(),
            line: Vec::new(),
            column: 0,
        }
    }

    fn push_segment(&mut self, length: usize, origin: Option<ColumnOrigin>) {
        let start = self.column;
        self.column += to_u32(length);
        let Some(origin) = origin else {
            return;
        };
        if length == 0 {
            return;
        }
        // a copied run that carries on from the one before it, in the input as in the output,
        // is one run
        if let Some(last) = self.line.last_mut()
            && last.end == start
        {
            let continues = match (last.origin, origin) {
                (
                    ColumnOrigin::Copied { line, column },
                    ColumnOrigin::Copied {
                        line: next_line,
                        column: next_column,
                    },
                ) => line == next_line && column + (last.end - last.start) == next_column,
                (ColumnOrigin::Qualifier, ColumnOrigin::Qualifier) => true,
                _ => false,
            };
            if continues {
                last.end = self.column;
                return;
            }
        }
        self.line.push(Segment {
            start,
            end: self.column,
            origin,
        });
    }

    fn newline(&mut self) {
        self.columns.push(std::mem::take(&mut self.line));
        self.column = 0;
    }

    /// `text`, copied from the input at `from`
    fn copied(&mut self, text: &str, from: usize) {
        let mut offset = from;
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                self.newline();
                offset += 1;
            }
            let (line, column) = position(&self.input_starts, offset);
            self.push_segment(piece.len(), Some(ColumnOrigin::Copied { line, column }));
            offset += piece.len();
        }
        debug_assert!(offset <= self.input.len());
    }

    /// `text`, written by the transpiler, as a qualifier or not
    fn written(&mut self, text: &str, qualifier: bool) {
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                self.newline();
            }
            self.push_segment(piece.len(), qualifier.then_some(ColumnOrigin::Qualifier));
        }
    }

    fn replacement(&mut self, replacement: &Replacement) {
        for (text, origin) in replacement.runs() {
            match origin {
                Origin::Copied(from) => self.copied(text, from),
                Origin::Generated(_) => self.written(text, false),
                Origin::Qualifier(_) => self.written(text, true),
            }
        }
    }

    fn finish(mut self, text: &str) -> Columns {
        // the last line, unless a final `\n` closed it and nothing followed
        if !text.is_empty() && !text.ends_with('\n') {
            self.columns.push(self.line);
        }
        self.columns
    }
}

/// `input` with `edits` made, and the result's columns against `input`
///
/// `edits` are ascending by start and do not overlap, in `input`'s byte offsets, as
/// [`crate::source_map::line_table`] takes them; insertions at one offset keep their order
pub(crate) fn edited(input: &str, edits: &[(usize, usize, Replacement)]) -> (String, Columns) {
    let mut text = String::with_capacity(input.len());
    let mut builder = Builder::new(input);
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        builder.copied(&input[cursor..*start], cursor);
        text.push_str(&input[cursor..*start]);
        builder.replacement(replacement);
        text.push_str(replacement.text());
        cursor = *end;
    }
    builder.copied(&input[cursor..], cursor);
    text.push_str(&input[cursor..]);
    let columns = builder.finish(&text);
    (text, columns)
}

/// `outer`, the columns of a text against an intermediate one, composed with `inner`, the
/// intermediate text's against an input: the text's columns against the input. a byte the
/// intermediate text's transpiler wrote stays the transpiler's
pub(crate) fn compose(outer: &[LineColumns], inner: &[LineColumns]) -> Columns {
    outer
        .iter()
        .map(|line| {
            let mut composed: LineColumns = Vec::new();
            for segment in line {
                match segment.origin {
                    ColumnOrigin::Qualifier => composed.push(*segment),
                    ColumnOrigin::Copied { line, column } => {
                        let Some(through) = inner.get(line as usize) else {
                            continue;
                        };
                        let length = segment.end - segment.start;
                        for piece in through {
                            let start = piece.start.max(column);
                            let end = piece.end.min(column + length);
                            if start >= end {
                                continue;
                            }
                            let origin = match piece.origin {
                                ColumnOrigin::Qualifier => ColumnOrigin::Qualifier,
                                ColumnOrigin::Copied { line, column: from } => {
                                    ColumnOrigin::Copied {
                                        line,
                                        column: from + (start - piece.start),
                                    }
                                }
                            };
                            composed.push(Segment {
                                start: segment.start + (start - column),
                                end: segment.start + (end - column),
                                origin,
                            });
                        }
                    }
                }
            }
            composed
        })
        .collect()
}

/// `columns`, the columns of `text` against `source`, with every copied run that is not the
/// bytes of `source` it claims to be left the transpiler's
pub(crate) fn verified(columns: Columns, text: &str, source: &str) -> Columns {
    let text_lines = lines(text);
    let source_lines = lines(source);
    columns
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let Some(written) = text_lines.get(index) else {
                return Vec::new();
            };
            line.into_iter()
                .filter(|segment| match segment.origin {
                    ColumnOrigin::Qualifier => written
                        .get(segment.start as usize..segment.end as usize)
                        .is_some(),
                    ColumnOrigin::Copied { line, column } => {
                        let claimed = written.get(segment.start as usize..segment.end as usize);
                        let length = (segment.end - segment.start) as usize;
                        let actual = source_lines.get(line as usize).and_then(|source| {
                            source.get(column as usize..column as usize + length)
                        });
                        claimed.is_some() && claimed == actual
                    }
                })
                .collect()
        })
        .collect()
}

/// `columns` with `count` lines the transpiler wrote inserted before line `at`
pub(crate) fn insert_written_lines(columns: &mut Columns, at: usize, count: usize) {
    let at = at.min(columns.len());
    columns.splice(at..at, std::iter::repeat_n(Vec::new(), count));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copied(start: u32, end: u32, line: u32, column: u32) -> Segment {
        Segment {
            start,
            end,
            origin: ColumnOrigin::Copied { line, column },
        }
    }

    #[test]
    fn an_unchanged_text_is_copied_from_where_it_is() {
        assert_eq!(
            identity("ab\n\ncd"),
            vec![vec![copied(0, 2, 0, 0)], vec![], vec![copied(0, 2, 2, 0)]]
        );
        assert_eq!(identity("ab\n"), vec![vec![copied(0, 2, 0, 0)]]);
        assert!(identity("").is_empty());
    }

    /// a name read through the lazy-import holder: the holder is a qualifier, and the name and
    /// the arguments after it are the author's
    #[test]
    fn a_qualified_name_keeps_the_author_s_columns() {
        let input = "p = Point(0.0, 0.0)\n";
        let mut qualified = Replacement::default();
        qualified.push_qualifier("_by_lazy.", 4);
        qualified.push_source(input, 4, 9);
        let (text, columns) = edited(input, &[(4, 9, qualified)]);
        assert_eq!(text, "p = _by_lazy.Point(0.0, 0.0)\n");
        assert_eq!(
            columns,
            vec![vec![
                copied(0, 4, 0, 0),
                Segment {
                    start: 4,
                    end: 13,
                    origin: ColumnOrigin::Qualifier
                },
                copied(13, 28, 0, 4),
            ]]
        );
    }

    /// a call the transpiler wrote around the author's expression is its own: only the
    /// expression inside is copied
    #[test]
    fn a_wrapping_call_is_the_transpiler_s() {
        let input = "for x in xs: pass\n";
        let mut wrapped = Replacement::generated("_soundness_iter(", 9);
        wrapped.push_source(input, 9, 11);
        wrapped.push_generated(")", 9);
        let (text, columns) = edited(input, &[(9, 11, wrapped)]);
        assert_eq!(text, "for x in _soundness_iter(xs): pass\n");
        assert_eq!(
            columns,
            vec![vec![
                copied(0, 9, 0, 0),
                copied(25, 27, 0, 9),
                copied(28, 34, 0, 11)
            ]]
        );
    }

    /// two steps compose: the second step's copy of the first's output lands on the input
    /// the first step copied it from
    #[test]
    fn steps_compose_onto_the_input() {
        let input = "a = f(x)\n";
        let mut wrapped = Replacement::generated("g(", 4);
        wrapped.push_source(input, 4, 8);
        wrapped.push_generated(")", 4);
        let (middle, first) = edited(input, &[(4, 8, wrapped)]);
        assert_eq!(middle, "a = g(f(x))\n");
        let (text, second) = edited(&middle, &[(0, 0, Replacement::generated("# top\n", 0))]);
        assert_eq!(text, "# top\na = g(f(x))\n");
        assert_eq!(
            compose(&second, &first),
            vec![vec![], vec![copied(0, 4, 0, 0), copied(6, 10, 0, 4)]]
        );
    }

    /// a step that only says which lines it left alone keeps those, and gives up the ones it
    /// changed
    #[test]
    fn a_changed_line_is_the_transpiler_s() {
        assert_eq!(
            equal_lines("a\nb\n", "a\nB\n"),
            vec![vec![copied(0, 1, 0, 0)], vec![]]
        );
        assert_eq!(equal_lines("a\n", "a\nb\n"), vec![vec![], vec![]]);
    }

    /// a claimed copy that is not the source's bytes is not reported
    #[test]
    fn a_copy_that_is_not_the_source_s_bytes_is_dropped() {
        let columns = vec![vec![copied(0, 3, 0, 0), copied(4, 5, 0, 4)]];
        assert_eq!(
            verified(columns, "abc x", "abz x"),
            vec![vec![copied(4, 5, 0, 4)]]
        );
    }
}
