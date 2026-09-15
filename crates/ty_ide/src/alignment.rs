//! which assignments share an `=` column, so a client drawing inlay hints can keep them sharing it
//!
//! an inlay hint costs horizontal room. drawn after the target of an assignment — which is where a
//! variable's type hint goes — it pushes everything to its right along, and a column of `=` the
//! author aligned by hand stops being a column:
//!
//! ```python
//! a     = [1, 2]     # `a: list[int]     = [1, 2]`
//! basdf = 1          # unchanged: a hint for a bare literal is suppressed
//! ```
//!
//! the client cannot repair that on its own by narrowing the hint, because the padding the author
//! wrote (five spaces) can be narrower than the hint that displaced it (eleven columns). restoring
//! the column means *widening* the other lines too, and to do that the client has to know which
//! lines were meant to be read together. that is a question about the source, so it is answered
//! here rather than guessed at from the text by every client in turn
//!
//! what is deliberately not decided here is how wide anything ends up. only the client knows which
//! hints are on screen at this instant — kinds can be switched off per editor, and push-to-hint
//! draws a hint only while a key is held — so a width computed here would be wrong the moment the
//! key came up. this module reports the grouping and the room the author left; the client turns
//! that into pixels
//!
//! a column is counted the way a fixed-width editor lays the line out: a wide character takes two
//! columns, a combining mark none, and a tab reaches the next multiple of the tab size — which is
//! the client's setting, so the client says what it is

use std::num::NonZeroU32;

use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, TraversalSignal, walk_body, walk_node,
};
use ruff_python_ast::{AnyNodeRef, Stmt};
use ruff_python_trivia::tab_offset_u32;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};
use unicode_width::UnicodeWidthChar;

use crate::Db;
use crate::inlay_hints::untyped_declaration_value;

/// one assignment in an alignment group
#[derive(Debug, Clone, Copy)]
pub struct AlignmentMember {
    /// where the run of spaces before the `=` begins
    ///
    /// this is the end of everything the line writes ahead of its column — the target of an
    /// assignment, or the name of a declaration whose type is left to be inferred
    ///
    /// it is *not* the one place a hint can land on this line. a line can carry several, and only
    /// their combined width says how far its `=` moves: `a, b = 1, 2` is hinted after `a` and again
    /// after `b`. the hints that displace this line's column are every hint drawn on this line at
    /// or before [`Self::gap_end`]
    pub gap_start: TextSize,

    /// the `=` the author lined up — the end of the run of spaces, and the column to preserve
    ///
    /// the distance back to [`Self::gap_start`] is the room a hint has to spend before the line has
    /// to grow. nothing here reads it: how much of that room a hint wants is the client's question,
    /// and since the column no longer has to be padded to be a column, it is not this module's
    /// either
    pub gap_end: TextSize,

    /// the display column [`Self::gap_start`] is at — how far along its line a fixed-width editor
    /// with the tab size given to [`alignment_groups`] draws it
    ///
    /// reported alongside the offset because a client lays hints out in columns, and a column is
    /// not something it can read back off the offset without repeating this measurement
    pub gap_start_column: u32,

    /// the display column of the `=`, which every member of a group shares
    ///
    /// the gap is spaces only, so this less [`Self::gap_start_column`] is also its length in
    /// characters
    pub gap_end_column: u32,
}

/// assignments the author put in one column, reported together because they have to move together
///
/// always two or more members, and always already sharing one `=` column — see
/// [`alignment_groups`]
#[derive(Debug)]
pub struct AlignmentGroup {
    pub members: Vec<AlignmentMember>,
}

/// the alignment groups with a line anywhere in `range`
///
/// a group is a run of assignments that are
///
/// - siblings in one suite, so an `if` in the middle ends the run rather than being aligned across
/// - unseparated by a blank line, which is how a reader tells one block of assignments from the
///   next (a comment on its own line does *not* break the run — it is still one block to read)
/// - and already sharing an `=` column, since a column that is not there yet is not one to preserve
///
/// nothing more is asked of the padding. a run of spaces before an `=` is what a *hand-aligned*
/// block looks like, and it is tempting to require one as proof the column was deliberate — but the
/// column is the thing being preserved, and this reads the same either way
///
/// ```python
/// a = 1 + 1
/// b = True or False
/// ```
///
/// those `=` are in one column because the names are the same length rather than because anyone
/// padded them, and a reader has no way to tell the two apart, nor any reason to want to. hints of
/// unequal width (`a: Literal[2]`, `b: Literal[True]`) break that column exactly as they break a
/// padded one, and leaving it broken is the surprise. so the shared column *is* the evidence, and
/// the padding a member carries is only room the client gets to spend before the line has to grow
///
/// the cost of taking the wider reading is a block where some lines have hints and some do not: the
/// hintless ones are pushed out to keep the column, having no hint of their own to narrow. that is
/// the same trade a padded block already makes, and it keeps the block square either way
///
/// a group is reported whole even when only one of its lines is in `range`: the column is a
/// property of every member at once, so half a group would be sized against the wrong maximum
///
/// `tab_size` is the number of columns between the tab stops of the editor the lines are shown in
pub fn alignment_groups(
    db: &dyn Db,
    file: PythonFile<'_>,
    range: TextRange,
    tab_size: NonZeroU32,
) -> Vec<AlignmentGroup> {
    let parsed = parsed_module(db, file).load(db);
    let source = source_text(db, file.file(db));
    let source = source.as_str();

    let mut visitor = AlignmentVisitor {
        source,
        range,
        tab_size,
        groups: Vec::new(),
    };
    walk_node(&mut visitor, AnyNodeRef::from(parsed.syntax()));

    visitor.groups.retain(|group| {
        group.members.iter().any(|member| {
            TextRange::new(
                source.line_start(member.gap_start),
                source.line_end(member.gap_end),
            )
            .intersect(range)
            .is_some()
        })
    });
    visitor.groups
}

/// what a statement does to the column running through its suite
#[derive(Debug, Clone, Copy)]
enum Alignment {
    /// the statement writes an `=` in the column and moves with it
    Member(AlignmentMember),

    /// the statement writes nothing in the column and competes with nothing there, so the block
    /// reads straight through it
    ///
    /// a declaration that only names a type — `verbose: bool` — is the case: its line stops short
    /// of the column, so widening the lines around it leaves it looking exactly as it was written
    Transparent,

    /// the statement ends the block
    ///
    /// `x += 1` is the one worth naming. it does write in the column, but what a reader lines up
    /// there is the whole `+=`, and the line carries no inferred type hint of its own — so it will
    /// sit still while its neighbours widen, and a block spanning it cannot be kept whole
    Breaks,
}

struct AlignmentVisitor<'a> {
    source: &'a str,

    /// the span the caller asked about, used to skip nodes it does not reach
    range: TextRange,

    /// the columns between tab stops, see [`display_column`]
    tab_size: NonZeroU32,

    groups: Vec<AlignmentGroup>,
}

impl<'a> SourceOrderVisitor<'a> for AlignmentVisitor<'a> {
    /// skips a node the request does not reach
    ///
    /// this cannot cut a group in half, because [`Self::visit_body`] collects a whole sibling list
    /// before the walk descends into any of it. skipping only stops the descent, and no group
    /// nested inside a node the range does not reach can itself reach the range
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.range.intersect(node.range()).is_some() {
            TraversalSignal::Traverse
        } else {
            TraversalSignal::Skip
        }
    }

    /// every suite, and only through this hook
    ///
    /// alignment is a property of *siblings*, and `visit_body` is the one place the traversal hands
    /// over a sibling list. watching `visit_stmt` instead would see the same statements with their
    /// nesting flattened away, and would happily align a module-level assignment with the first
    /// line of a function body underneath it
    fn visit_body(&mut self, body: &'a [Stmt]) {
        self.collect(body);
        walk_body(self, body);
    }
}

impl AlignmentVisitor<'_> {
    /// cuts one suite into runs of assignments that share a column, and keeps the ones with
    /// somebody to share it with
    fn collect(&mut self, body: &[Stmt]) {
        let mut run: Vec<AlignmentMember> = Vec::new();
        // the end of the last *member*, which is where the search for a blank line starts. it does
        // not advance over a transparent statement, so a blank line on either side of one is still
        // a blank line between the members it sits among
        let mut previous_end: Option<TextSize> = None;

        for stmt in body {
            let member = match self.alignment_of(stmt) {
                Alignment::Member(member) => member,
                Alignment::Transparent => continue,
                Alignment::Breaks => {
                    self.flush(&mut run);
                    previous_end = None;
                    continue;
                }
            };

            let joins = match (run.first(), previous_end) {
                (Some(first), Some(previous_end)) => {
                    first.gap_end_column == member.gap_end_column
                        && !self.blank_line_between(previous_end, stmt.start())
                }
                _ => false,
            };
            if !joins {
                self.flush(&mut run);
            }
            run.push(member);
            previous_end = Some(stmt.end());
        }
        self.flush(&mut run);
    }

    /// keeps a finished run if it is a group worth reporting, and starts the next one either way
    ///
    /// two members is the whole of the test. a run got this far by sharing a column, and a column is
    /// a column however it came to be one — see [`alignment_groups`] on why the padding a member
    /// happens to carry is not the evidence it looks like
    fn flush(&mut self, run: &mut Vec<AlignmentMember>) {
        if run.len() >= 2 {
            self.groups.push(AlignmentGroup {
                members: std::mem::take(run),
            });
        } else {
            run.clear();
        }
    }

    /// what this statement does to the column running through its suite
    ///
    /// `Assign` and `AnnAssign` both hold a column. an annotated assignment never carries an
    /// inferred type hint — the type is written out — but it still occupies the column, so leaving
    /// it out would let a group be sized against a line that is not in it
    fn alignment_of(&self, stmt: &Stmt) -> Alignment {
        let gap_start = match stmt {
            Stmt::Assign(assign) => match assign.targets.as_slice() {
                [target] => target.end(),
                // `a = b = 1` has two columns and no way to say which was aligned
                _ => return Alignment::Breaks,
            },
            Stmt::AnnAssign(assign) => {
                // basedpython: a declaration that leaves its type to be inferred — `let a = v`,
                // `var a = v` — parses as an annotated assignment whose annotation is a synthetic
                // marker spanning the keyword prefix, and that marker *ends before the name*. what
                // the line writes ahead of its column is the name, so the gap starts there — which
                // is also where the hint for the inferred type is drawn. a declaration that does
                // name a type keeps it under the marker (`let a: T = v` parses as
                // `a: __let__[T] = v`), and there the annotation ends at the written type as usual
                if untyped_declaration_value(assign).is_some() {
                    assign.target.end()
                } else if assign.value.is_some() {
                    assign.annotation.end()
                } else {
                    // no value is no `=`, and a line that stops short of the column neither holds
                    // it nor breaks it
                    return Alignment::Transparent;
                }
            }
            _ => return Alignment::Breaks,
        };
        match self.equals_after(gap_start) {
            Some(gap_end) => Alignment::Member(AlignmentMember {
                gap_start,
                gap_end,
                gap_start_column: display_column(self.source, gap_start, self.tab_size),
                gap_end_column: display_column(self.source, gap_end, self.tab_size),
            }),
            None => Alignment::Breaks,
        }
    }

    /// the `=` that follows `from` with nothing but spaces in between, if that is what follows
    ///
    /// spaces only, and deliberately. the gap is room a hint spends column for column, and a tab
    /// is not that: how far it reaches depends on where it starts, so a hint drawn ahead of it
    /// changes how much room it leaves. a newline or a `\` means the `=` is on another line, where
    /// it is aligned with nothing. each of those makes the statement unalignable rather than merely
    /// unpadded
    fn equals_after(&self, from: TextSize) -> Option<TextSize> {
        let rest = self.source.get(from.to_usize()..)?;
        let spaces = rest.len() - rest.trim_start_matches(' ').len();
        if rest.as_bytes().get(spaces)? != &b'=' {
            return None;
        }
        Some(from + TextSize::try_from(spaces).ok()?)
    }

    /// whether the author left an empty line between two statements
    ///
    /// the first newline is the one that ends the earlier statement. any *complete* line after it
    /// that holds nothing but whitespace is a blank line, and a blank line is where one block of
    /// assignments stops and another starts. lines with a comment on them are not blank and do not
    /// break the block, which matches how a run of settings tends to get annotated
    fn blank_line_between(&self, from: TextSize, to: TextSize) -> bool {
        let Some(between) = self.source.get(from.to_usize()..to.to_usize()) else {
            return true;
        };
        let Some(first) = between.find('\n') else {
            return false;
        };
        let rest = &between[first + 1..];
        // `split` yields one more piece than there are newlines; the last is the partial line the
        // next statement's indentation sits on, and a partial line cannot be blank
        let complete = rest.matches('\n').count();
        rest.split('\n')
            .take(complete)
            .any(|line| line.trim().is_empty())
    }
}

/// how far along its line a fixed-width editor draws `offset`
///
/// neither bytes nor characters answer that. `é = 1` and `ab = 1` spend the same number of bytes
/// reaching their `=` and put it in different columns; `名前 = 1` and `abcd = 1` spend different
/// numbers of characters and put it in the same one. so each character counts for its unicode
/// width — two for a wide character, none for a combining mark — and a tab advances to the next
/// multiple of `tab_size`, the same measure `ruff_linter`'s `LineWidthBuilder` gives a line
///
/// still columns rather than pixels: in a proportional font even two ASCII letters differ, and
/// that is the client's to draw. what is decided here is whether the author put two `=` in the
/// same place on a fixed grid
fn display_column(source: &str, offset: TextSize, tab_size: NonZeroU32) -> u32 {
    let line_start = source.line_start(offset);
    source
        .get(line_start.to_usize()..offset.to_usize())
        .unwrap_or_default()
        .chars()
        .fold(0, |column, character| match character {
            '\t' => column + tab_offset_u32(column, tab_size.get()),
            #[expect(clippy::cast_possible_truncation)]
            character => column + character.width().unwrap_or(0) as u32,
        })
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_source_file::{LineIndex, OneIndexed};

    use super::*;
    use crate::tests::{CursorTest, cursor_test};

    impl CursorTest {
        /// the groups, drawn over the lines they were found on
        ///
        /// rendered rather than asserted as offsets because what matters about this analysis is
        /// which lines ended up together and how much room each has, and a list of byte offsets
        /// hides both
        ///
        /// with tab stops every four columns
        fn alignment_groups(&self) -> String {
            self.alignment_groups_with_tab_size(4)
        }

        /// the same, with tab stops every `tab_size` columns
        fn alignment_groups_with_tab_size(&self, tab_size: u32) -> String {
            let source = self.cursor.source.as_str();
            self.alignment_groups_in(
                TextRange::new(TextSize::new(0), TextSize::of(source)),
                tab_size,
            )
        }

        /// the same, for a caller asking about only part of the file
        ///
        /// each line is drawn with its tabs expanded, and the gap under it at the columns the
        /// analysis reported, so a column it got wrong shows up as a gap drawn in the wrong place
        fn alignment_groups_in(&self, range: TextRange, tab_size: u32) -> String {
            use std::fmt::Write;

            let source = self.cursor.source.as_str();
            let tab_size = NonZeroU32::new(tab_size).unwrap();
            let groups = alignment_groups(
                &self.db,
                self.program_file(self.cursor.file).python_file(&self.db),
                range,
                tab_size,
            );
            if groups.is_empty() {
                return "no groups".to_string();
            }

            let mut out = String::new();
            for (index, group) in groups.iter().enumerate() {
                writeln!(out, "group {}", index + 1).unwrap();
                for member in &group.members {
                    let start = source.line_start(member.gap_start);
                    let line = source[start.to_usize()..]
                        .split('\n')
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches('\r');
                    // the same walk `display_column` makes, character by character: an oracle
                    // that measured the line another way could agree with a wrong answer
                    let mut expanded = String::new();
                    let mut width = 0u32;
                    for character in line.chars() {
                        if character == '\t' {
                            let stop = tab_offset_u32(width, tab_size.get());
                            expanded.extend(std::iter::repeat_n(' ', stop as usize));
                            width += stop;
                        } else {
                            expanded.push(character);
                            width += u32::try_from(character.width().unwrap_or(0)).unwrap();
                        }
                    }
                    let lead = member.gap_start_column as usize;
                    let gap = (member.gap_end_column - member.gap_start_column) as usize;
                    writeln!(out, "  {expanded}").unwrap();
                    writeln!(
                        out,
                        "  {}{} gap {gap}",
                        " ".repeat(lead),
                        "-".repeat(gap.max(1)),
                    )
                    .unwrap();
                }
            }
            out
        }

        /// the range of one line, for the tests that ask about only part of a file
        fn line(&self, zero_indexed: usize) -> TextRange {
            let source = self.cursor.source.as_str();
            LineIndex::from_source_text(source)
                .line_range(OneIndexed::from_zero_indexed(zero_indexed), source)
        }
    }

    /// a single-file test over basedpython source, where a binding can be written with `let`
    fn by_test(source: &str) -> CursorTest {
        CursorTest::builder().source("main.by", source).build()
    }

    #[test]
    fn aligns_a_padded_column() {
        let test = cursor_test(
            "\
a     = 1 + 1
basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a     = 1 + 1
           ----- gap 5
          basdf = 1
               - gap 1
        ");
    }

    #[test]
    fn an_unpadded_column_is_still_a_column() {
        let test = cursor_test(
            "\
a = 1 + 1
b = True or False
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a = 1 + 1
           - gap 1
          b = True or False
           - gap 1
        ");
    }

    #[test]
    fn a_column_can_have_no_gap_at_all() {
        // no spaces is a gap of nought rather than no member: the `=` still share a column, and a
        // member left out would leave the group sized against the wrong maximum. only reachable now
        // that a run no longer needs a padded member to qualify
        let test = cursor_test(
            "\
ab=f()
a =1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          ab=f()
            - gap 0
          a =1
           - gap 1
        ");
    }

    #[test]
    fn a_column_of_one_is_no_column() {
        let test = cursor_test(
            "\
x = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    #[test]
    fn a_blank_line_ends_the_block() {
        let test = cursor_test(
            "\
a     = 1 + 1
basdf = 1

cc    = 2 + 2
dddde = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a     = 1 + 1
           ----- gap 5
          basdf = 1
               - gap 1
        group 2
          cc    = 2 + 2
            ---- gap 4
          dddde = 3
               - gap 1
        ");
    }

    #[test]
    fn a_comment_line_does_not() {
        let test = cursor_test(
            "\
a     = 1 + 1
# why
basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a     = 1 + 1
           ----- gap 5
          basdf = 1
               - gap 1
        ");
    }

    #[test]
    fn a_differing_column_starts_a_new_group() {
        let test = cursor_test(
            "\
a     = 1 + 1
basdf = 1
cc      = 2
ddddddd = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a     = 1 + 1
           ----- gap 5
          basdf = 1
               - gap 1
        group 2
          cc      = 2
            ------ gap 6
          ddddddd = 3
                 - gap 1
        ");
    }

    #[test]
    fn suites_do_not_align_across_nesting() {
        let test = cursor_test(
            "\
def f():
    a     = 1 + 1
    basdf = 1

aaaaaaaaa = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
              a     = 1 + 1
               ----- gap 5
              basdf = 1
                   - gap 1
        ");
    }

    #[test]
    fn an_annotated_assignment_holds_the_column() {
        let test = cursor_test(
            "\
a: int   = 1
bbbbbb   = 2 + 2
cc       = 3 + 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a: int   = 1
                --- gap 3
          bbbbbb   = 2 + 2
                --- gap 3
          cc       = 3 + 3
            ------- gap 7
        ");
    }

    #[test]
    fn a_multiline_value_does_not_break_the_block() {
        let test = cursor_test(
            "\
a     = foo(
    1,
)
basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a     = foo(
           ----- gap 5
          basdf = 1
               - gap 1
        ");
    }

    #[test]
    fn a_statement_between_ends_the_block() {
        let test = cursor_test(
            "\
a     = 1 + 1
print(a)
basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// `let a = v` writes no type and is hinted with the one that was inferred, which makes it the
    /// case this whole analysis exists for. the parser models the `let` as an annotation spanning
    /// the keyword, so the gap has to be measured from the name rather than from that
    #[test]
    fn a_declaration_that_infers_its_type_holds_the_column() {
        let test = by_test(
            "\
let a     = [1, 2]
let basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          let a     = [1, 2]
               ----- gap 5
          let basdf = 1
                   - gap 1
        ");
    }

    #[test]
    fn a_var_declaration_holds_it_too() {
        let test = by_test(
            "\
var a     = [1, 2]
var basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          var a     = [1, 2]
               ----- gap 5
          var basdf = 1
                   - gap 1
        ");
    }

    /// a declaration that names its type keeps it under the marker, so the gap runs from the type
    #[test]
    fn a_declaration_that_names_its_type_holds_the_column() {
        let test = by_test(
            "\
let a: int    = 1
let bb: str   = 2
let ccc: bool = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          let a: int    = 1
                    ---- gap 4
          let bb: str   = 2
                     --- gap 3
          let ccc: bool = 3
                       - gap 1
        ");
    }

    /// a declaration sits in a block of plain assignments without ending it
    #[test]
    fn a_declaration_mixes_with_a_plain_assignment() {
        let test = by_test(
            "\
aaaaaaa   = [1, 2]
let basdf = 1
ccccccc   = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          aaaaaaa   = [1, 2]
                 --- gap 3
          let basdf = 1
                   - gap 1
          ccccccc   = 2
                 --- gap 3
        ");
    }

    /// an unpacking writes one column but is hinted in more than one place, so the member reports
    /// where the padding starts and the client adds up every hint on the line
    #[test]
    fn an_unpacking_reports_the_end_of_its_target() {
        let test = cursor_test(
            "\
a, b  = 1, 2
basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          a, b  = 1, 2
              -- gap 2
          basdf = 1
               - gap 1
        ");
    }

    /// a column is where a reader sees the `=`, so it is not counted in bytes. these two spend the
    /// same number of bytes reaching their `=` and put it in different columns
    #[test]
    fn a_column_is_not_a_byte_count() {
        let test = cursor_test(
            "\
é    = 1
abcd  = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// and the same the other way: these two put their `=` in one column while spending different
    /// numbers of bytes to get there
    #[test]
    fn a_column_of_characters_is_still_a_column() {
        let test = cursor_test(
            "\
é     = 1
abcde = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          é     = 1
           ----- gap 5
          abcde = 2
               - gap 1
        ");
    }

    /// a wide character takes two columns of the line, so these two put their `=` in one column
    /// while spending different numbers of characters to get there
    #[test]
    fn a_wide_character_takes_two_columns() {
        let test = cursor_test(
            "\
名前 = 1
abcd = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          名前 = 1
              - gap 1
          abcd = 2
              - gap 1
        ");
    }

    /// and these spend the same number of characters and put their `=` in different columns
    #[test]
    fn a_column_is_not_a_character_count() {
        let test = cursor_test(
            "\
名前   = 1
abcd = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// a tab before the gap reaches the next tab stop, so where the `=` lands depends on it
    #[test]
    fn a_tab_in_the_target_reaches_the_next_tab_stop() {
        let test = cursor_test(
            "\
x:\tint = 1
abcdefg = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          x:  int = 1
                 - gap 1
          abcdefg = 2
                 - gap 1
        ");
    }

    /// and the tab stop is the editor's, so the same lines with stops every eight columns put the
    /// first `=` four columns further out
    #[test]
    fn the_tab_stop_is_the_editors() {
        let test = cursor_test(
            "\
x:\tint = 1
abcdefg = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups_with_tab_size(8), @"no groups");
    }

    /// indentation is measured with the rest of the line, so a suite indented with tabs lines up
    /// the same as one indented with spaces
    #[test]
    fn a_suite_indented_with_tabs() {
        let test = cursor_test(
            "\
def f():
\ta     = 1
\tbasdf = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
              a     = 1
               ----- gap 5
              basdf = 2
                   - gap 1
        ");
    }

    /// a declaration with no value writes nothing in the column, so the block reads through it
    #[test]
    fn a_declaration_without_a_value_does_not_end_the_block() {
        let test = cursor_test(
            "\
debug   = False
verbose: bool
retries = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @r"
        group 1
          debug   = False
               --- gap 3
          retries = 3
                 - gap 1
        ");
    }

    /// but a blank line on either side of one still separates the blocks
    #[test]
    fn a_blank_line_around_a_valueless_declaration_still_does() {
        let test = cursor_test(
            "\
debug   = False

verbose: bool
retries = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// what a reader lines up on an augmented assignment is the whole `+=`, and the line carries no
    /// inferred hint to move it, so a block cannot be kept across one
    #[test]
    fn an_augmented_assignment_ends_the_block() {
        let test = cursor_test(
            "\
a      = 1
basdf += 1
cc     = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// a chain has two columns and no way to say which one was aligned
    #[test]
    fn a_chained_assignment_ends_the_block() {
        let test = cursor_test(
            "\
aaaaaa    = 1
bb    = c = 2
dddddd    = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// a tab in the gap reaches further or less far depending on where the hint ahead of it ends, so
    /// it is not room a hint can spend
    #[test]
    fn a_tab_before_the_equals_ends_the_block() {
        let test = cursor_test(
            "\
a\t\t= 1
basdf = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// and an `=` carried onto the next line is aligned with nothing
    #[test]
    fn a_continued_line_ends_the_block() {
        let test = cursor_test(
            "\
a \\
      = 1
basdf = 2
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups(), @"no groups");
    }

    /// a group is reported whole when any one of its lines is asked about, because the column is
    /// sized against every member at once
    #[test]
    fn one_line_in_range_reports_the_whole_group() {
        let test = cursor_test(
            "\
a     = 1
basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups_in(test.line(1), 4), @r"
        group 1
          a     = 1
           ----- gap 5
          basdf = 1
               - gap 1
        ");
    }

    /// a narrow range still reaches a group nested inside a suite, which is what the walk skipping
    /// whole nodes it does not reach has to leave alone
    #[test]
    fn a_narrow_range_still_reaches_a_nested_group() {
        let test = cursor_test(
            "\
def f():
    a     = 1
    basdf = 1
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups_in(test.line(1), 4), @r"
        group 1
              a     = 1
               ----- gap 5
              basdf = 1
                   - gap 1
        ");
    }

    /// and a group none of whose lines are asked about is left out
    #[test]
    fn a_group_outside_the_range_is_left_out() {
        let test = cursor_test(
            "\
a     = 1
basdf = 1

cc    = 2
dddde = 3
<CURSOR>",
        );
        assert_snapshot!(test.alignment_groups_in(test.line(3), 4), @r"
        group 1
          cc    = 2
            ---- gap 4
          dddde = 3
               - gap 1
        ");
    }
}
