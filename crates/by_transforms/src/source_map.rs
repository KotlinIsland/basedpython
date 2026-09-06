//! The output-line → input-line table for one edit-application pass, and the
//! replacement type that lets it say where each line of a replacement came from.
//!
//! An edit's replacement is not one thing: a template re-emits spans of the
//! source it rewrites — a trailing-lambda block's whole suite, the call it hangs
//! off — around text the lowering wrote itself. Reading the replacement as a
//! string loses that, and every line of it then has to be charged to the one
//! line the edit started on: a traceback inside a hoisted block would name the
//! statement that owns the block instead of the line that raised. So a
//! [`Replacement`] keeps the runs it was assembled from, and the table charges a
//! copied run to the line it was copied from and generated text to the construct
//! it stands for.

/// Where one run of a replacement's text came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// copied verbatim from the source, starting at this byte offset
    Copied(usize),
    /// written by a lowering; it stands for the construct at this source offset
    /// (the start of the edit that wrote it), which is the one `.by` position a
    /// reader can be pointed at for text no source spells
    Generated(usize),
}

/// The replacement text of one edit, remembered as the runs it was assembled
/// from: spans copied out of the source, and text a lowering generated.
///
/// The runs are what a line table is built from. They do not change what is
/// written: [`text`](Self::text) is the replacement, exactly as it would have
/// been assembled into a plain string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Replacement {
    text: String,
    /// `(offset into `text`, origin)` of each run, ascending; a run extends to
    /// the next run's start, or to the end of the text. never an empty run, and
    /// contiguous copies (and generated runs with one anchor) are merged, so
    /// two replacements assembled from different fragments compare equal when
    /// they say the same thing
    runs: Vec<(usize, Origin)>,
}

impl Replacement {
    /// Text no source span spells, standing for the construct at `anchor` — a
    /// re-rendered statement, a plain-text substitution.
    pub(crate) fn generated(text: &str, anchor: usize) -> Self {
        let mut replacement = Self::default();
        replacement.push_generated(text, anchor);
        replacement
    }

    /// Append text a lowering wrote, standing for the construct at `anchor`.
    pub(crate) fn push_generated(&mut self, text: &str, anchor: usize) {
        if text.is_empty() {
            return;
        }
        let origin = Origin::Generated(anchor);
        if self.runs.last().is_none_or(|&(_, last)| last != origin) {
            self.runs.push((self.text.len(), origin));
        }
        self.text.push_str(text);
    }

    /// Append `source[start..end]` verbatim.
    pub(crate) fn push_source(&mut self, source: &str, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let continues = self.runs.last().is_some_and(|&(at, origin)| {
            matches!(origin, Origin::Copied(from) if from + (self.text.len() - at) == start)
        });
        if !continues {
            self.runs.push((self.text.len(), Origin::Copied(start)));
        }
        self.text.push_str(&source[start..end]);
    }

    /// The replacement text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The runs, as `(text, origin)`, in order.
    fn runs(&self) -> impl Iterator<Item = (&str, Origin)> {
        self.runs.iter().enumerate().map(|(i, &(at, origin))| {
            let end = self
                .runs
                .get(i + 1)
                .map_or(self.text.len(), |&(next, _)| next);
            (&self.text[at..end], origin)
        })
    }
}

/// The byte offsets `source`'s lines start at, for turning an offset into a
/// line.
///
/// Lines are delimited by `\n` alone, as everything downstream counts them: the
/// table is indexed by python's line numbers, and a table that broke lines
/// differently from the text it describes would be off by one from that point
/// on.
struct LineStarts(Vec<usize>);

impl LineStarts {
    fn of(source: &str) -> Self {
        Self(
            std::iter::once(0)
                .chain(source.match_indices('\n').map(|(i, _)| i + 1))
                .collect(),
        )
    }

    /// The 0-based line `offset` is on. An offset at or past the end of the
    /// source is on the last line.
    fn line_of(&self, offset: usize) -> u32 {
        let index = self.0.partition_point(|&start| start <= offset);
        u32::try_from(index.saturating_sub(1)).unwrap_or(u32::MAX)
    }
}

/// The table under construction: one entry per completed output line, plus
/// what is known about the line being built.
///
/// A line is charged to the first visible *source* text on it, and only failing
/// that to the construct the first visible generated text on it stands for.
/// Generated text is mostly glue around copied operands — the indentation ahead
/// of a re-emitted call, `type(` around a receiver, `cell.append(` around a
/// returned value — and the operand is the thing the reader wants named. A line
/// whose visible text is all generated (a hoisted `def` header, a `nonlocal`, an
/// injected keyword argument on a line of its own) is charged to the edit that
/// wrote it. Whitespace charges nothing: the indentation copied from the line a
/// call's closing paren sat on says nothing about the keyword written after it.
///
/// Charged when the line is *opened* rather than when it is closed: the `\n`
/// that closes a line may be copied from a source line other than the one the
/// line's text came from — the newline after a comment that trails a moved
/// block, say — and it says nothing about the text before it. A line holding no
/// visible text at all is the line of whatever run terminates it, so a blank
/// line copied from the source maps to itself.
#[derive(Default)]
struct Table {
    lines: Vec<Option<u32>>,
    /// the source line of the first visible copied text on the open line
    copied: Option<u32>,
    /// the anchor line of the first visible generated text on the open line
    generated: Option<u32>,
}

impl Table {
    /// Append `text`, charged to `line` — the line it was copied from (advancing
    /// as the text does) when `copied`, and otherwise the line it stands for.
    fn push(&mut self, text: &str, mut line: u32, copied: bool) {
        for byte in text.bytes() {
            if byte == b'\n' {
                let origin = self.copied.or(self.generated).unwrap_or(line);
                self.lines.push(Some(origin));
                self.copied = None;
                self.generated = None;
                if copied {
                    line += 1;
                }
            } else if byte.is_ascii_whitespace() {
                continue;
            } else if copied {
                self.copied.get_or_insert(line);
            } else {
                self.generated.get_or_insert(line);
            }
        }
    }

    /// The table, with an entry for a last line nothing terminated.
    fn finish(mut self) -> Vec<Option<u32>> {
        if let Some(origin) = self.copied.or(self.generated) {
            self.lines.push(Some(origin));
        }
        self.lines
    }
}

/// Build an output-line → input-line table for a single edit-application pass.
///
/// `edits` must be ascending by start and non-overlapping, expressed in `source`
/// byte coordinates (the same shape `replace_range` is fed). Each output *line*
/// maps to the input line it came from: a line copied from the source to that
/// line, a line of a replacement to the line of the source text it re-emits —
/// or, where it re-emits none, to the line of the construct the edit rewrote
/// (see `Table`). This is the line-level primitive the run-time traceback
/// rewriter composes; column-accurate mapping is future work (see
/// `docs/basedpython/development/sourcemaps.md`).
///
/// A line, and not a `\n`. The two only differ at the end of a file that has no
/// terminator on its last line, and there the difference is the whole thing: that
/// last line is a line, python numbers it, a traceback names it, and a debugger
/// asks this table what `.by` line is behind it. Counting terminators drops it,
/// and — because the table is indexed by generated line — the answer that comes
/// back is not "unknown" but the entry that belongs to the line *after* it, or
/// nothing at all when it was the last entry. Everything downstream is written to
/// that contract already: `extension::hoist_backing_functions` refuses to reorder a
/// table whose length is not the output's line count, and `_by_sourcemap.py`'s
/// `None` means "prelude, no `.by` line is behind this" — which is a lie told about
/// a user's own code if their file simply ended without a newline.
///
/// (Named rather than linked: this is public and that is not, and rustdoc rejects a
/// link from one to the other.)
pub(crate) fn line_table(source: &str, edits: &[(usize, usize, Replacement)]) -> Vec<Option<u32>> {
    let starts = LineStarts::of(source);
    let mut table = Table::default();
    let mut src_pos = 0usize;
    for (start, end, replacement) in edits {
        table.push(&source[src_pos..*start], starts.line_of(src_pos), true);
        for (text, origin) in replacement.runs() {
            match origin {
                Origin::Copied(from) => table.push(text, starts.line_of(from), true),
                Origin::Generated(anchor) => table.push(text, starts.line_of(anchor), false),
            }
        }
        src_pos = *end;
    }
    table.push(&source[src_pos..], starts.line_of(src_pos), true);
    table.finish()
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::{Replacement, line_table};

    /// The count that everything downstream indexes by. Stated as a property
    /// over both end-of-file shapes, because the whole bug this guards was a
    /// table that was right for one of them and one entry short for the other.
    fn line_count(text: &str) -> usize {
        if text.is_empty() {
            0
        } else {
            text.lines().count()
        }
    }

    /// a last line with nothing after it is still a line — python numbers it, a
    /// traceback names it, and the debugger asks this table about it
    #[test]
    fn a_file_that_ends_without_a_newline_still_maps_its_last_line() {
        assert_eq!(
            line_table("a = 1\nb = 2", &[]),
            vec![Some(0), Some(1)],
            "the unterminated last line is the second entry"
        );
    }

    /// and the terminated spelling of the same file maps identically: the `\n`
    /// closes the line, it does not add one
    #[test]
    fn a_trailing_newline_adds_no_entry_of_its_own() {
        assert_eq!(
            line_table("a = 1\nb = 2\n", &[]),
            line_table("a = 1\nb = 2", &[])
        );
    }

    /// an empty file has no lines to map, which is not the same as one blank one
    #[test]
    fn an_empty_source_maps_nothing() {
        assert!(line_table("", &[]).is_empty());
        assert_eq!(line_table("\n", &[]), vec![Some(0)]);
    }

    /// the table is indexed by output line, so it has to have exactly as many
    /// entries as the output has lines — the invariant
    /// `hoist_backing_functions` refuses to reorder a table without
    #[test]
    fn the_table_has_one_entry_per_output_line() {
        for source in [
            "",
            "a = 1",
            "a = 1\n",
            "a = 1\nb = 2",
            "a = 1\nb = 2\n",
            "\n\n\n",
            "a = 1\n\n",
        ] {
            assert_eq!(
                line_table(source, &[]).len(),
                line_count(source),
                "one entry per line of {source:?}"
            );
        }
    }

    /// an edit that lands on the unterminated last line does not cost it its
    /// entry, whether the replacement is wider, narrower or multi-line
    #[test]
    fn an_edit_on_the_last_line_keeps_it_mapped() {
        // `b = 2` → `b = 22`, no newline anywhere near it
        let widened = line_table(
            "a = 1\nb = 2",
            &[(10, 11, Replacement::generated("22", 10))],
        );
        assert_eq!(widened, vec![Some(0), Some(1)]);

        // a replacement that brings its own line break: both generated lines
        // are attributed to the `.by` line the edit started on
        let split = line_table(
            "a = 1\nb = 2",
            &[(6, 11, Replacement::generated("b = 2\nc = 3", 6))],
        );
        assert_eq!(split, vec![Some(0), Some(1), Some(1)]);
    }

    /// generated text has no line of its own, so every line of it is charged
    /// to the construct it was written for — and the text after the edit goes
    /// on mapping to its own lines, wherever the replacement left off
    #[test]
    fn a_generated_replacement_is_charged_to_the_construct_it_rewrote() {
        let source = "a = 1\nb = 2\nc = 3\n";
        let table = line_table(
            source,
            &[(6, 11, Replacement::generated("x = 0\ny = 0\nz = 0", 6))],
        );
        assert_eq!(table, vec![Some(0), Some(1), Some(1), Some(1), Some(2)]);
    }

    /// the shape a trailing-lambda block lowers to: the suite is hoisted into a
    /// `def` ahead of the call it hung off, so the replacement re-emits source
    /// out of order. each copied line maps to the line it was copied from, the
    /// generated `def` header to the block statement, and the re-emitted call
    /// to its own line — not to the line of the suite's last statement, which
    /// is where the text after the edit resumes
    #[test]
    fn copied_runs_map_to_the_lines_they_were_copied_from() {
        let source = "f(1):\n    print(it)\n    raise E  # note\nprint(2)\n";
        let call_end = 3; // `f(1` — the trailing argument goes before the `)`
        let suite_start = 5; // just past the `:`
        let suite_end = 31; // end of `raise E`, ahead of the trailing comment
        assert_eq!(&source[..call_end], "f(1");
        assert_eq!(
            &source[suite_start..suite_end],
            "\n    print(it)\n    raise E"
        );

        let mut replacement = Replacement::default();
        replacement.push_generated("def _trailing_lambda_0(it=None):", 0);
        replacement.push_source(source, suite_start, suite_end);
        replacement.push_generated("\n", 0);
        replacement.push_source(source, 0, call_end);
        replacement.push_generated(", a=_trailing_lambda_0)", 0);
        assert_eq!(
            format!("{}{}", replacement.text(), &source[suite_end..]),
            "def _trailing_lambda_0(it=None):\n    print(it)\n    raise E\nf(1, a=_trailing_lambda_0)  # note\nprint(2)\n",
            "the replacement is the text a plain string would have carried"
        );

        assert_eq!(
            line_table(source, &[(0, suite_end, replacement)]),
            vec![Some(0), Some(1), Some(2), Some(0), Some(3)],
            "def header → the block statement; suite lines → themselves; the call → itself"
        );
    }

    /// a line that opens with generated glue and goes on with copied text is
    /// the copied text's line: the indentation ahead of a re-emitted call says
    /// nothing, the call does
    #[test]
    fn copied_text_outranks_the_glue_around_it() {
        let source = "x = 1\ny = 2\n";
        let mut replacement = Replacement::default();
        replacement.push_generated("pre\n    ", 0);
        replacement.push_source(source, 6, 11);
        replacement.push_generated("  # gen", 0);
        // the first output line is generated only, so it is the anchor's; the
        // second opens with generated indentation but holds `y = 2`, copied
        // from line 1
        assert_eq!(
            line_table(source, &[(0, 11, replacement)]),
            vec![Some(0), Some(1)]
        );
    }

    /// a replacement's runs describe the same text a plain string would carry,
    /// however the runs were pushed — contiguous copies and same-anchor
    /// generated text merge, so two assemblies of one text compare equal
    #[test]
    fn runs_are_canonical() {
        let source = "abcdef";
        let mut piecewise = Replacement::default();
        piecewise.push_source(source, 0, 2);
        piecewise.push_source(source, 2, 4);
        piecewise.push_generated("X", 4);
        piecewise.push_generated("", 4);
        piecewise.push_generated("Y", 4);
        let mut whole = Replacement::default();
        whole.push_source(source, 0, 4);
        whole.push_generated("XY", 4);
        assert_eq!(piecewise, whole);
        assert_eq!(whole.text(), "abcdXY");
        assert!(Replacement::default().text().is_empty());

        // a copy that does not continue the last one is a run of its own, and
        // the line it opens is charged to the first copy
        let mut skipped = Replacement::default();
        skipped.push_source(source, 0, 2);
        skipped.push_source(source, 3, 5);
        assert_eq!(skipped.text(), "abde");
        assert_eq!(line_table("ab\ncd\ne", &[(0, 7, skipped)]), vec![Some(0)]);
    }

    /// an insertion that adds whole lines ahead of a statement leaves that
    /// statement mapped to itself
    #[test]
    fn an_insertion_does_not_shift_the_line_it_precedes() {
        let source = "a = 1\nb = 2\n";
        let edits = [(6, 6, Replacement::generated("g = 0\n", 6))];
        assert_eq!(line_table(source, &edits), vec![Some(0), Some(1), Some(1)]);
    }

    /// Every generated line beside the `.by` line it maps to, from the first
    /// line that maps to source onwards: what precedes it is the import
    /// preamble, whose length is not what these tests pin.
    fn mapped_lines(source: &str) -> Vec<(Option<u32>, String)> {
        let (db, file) = crate::make_in_memory_db(source);
        let (output, map) =
            crate::transpile_typed_with_map(&db, file, &crate::Config::test_default(), None)
                .expect("transpile failed");
        assert_eq!(
            map.len(),
            output.lines().count(),
            "one entry per generated line:\n{output}"
        );
        let first = map
            .iter()
            .position(Option::is_some)
            .expect("some line maps to source");
        output
            .lines()
            .zip(map)
            .skip(first)
            .map(|(line, mapped)| (mapped, line.to_owned()))
            .collect()
    }

    /// The whole map past the preamble, as `(.by line, generated text)` pairs:
    /// every line has to be right, not only the one a test happens to look up,
    /// because a traceback or a breakpoint can land on any of them.
    #[track_caller]
    fn assert_mapped(source: &str, expected: &[(u32, &str)]) {
        let expected: Vec<(Option<u32>, String)> = expected
            .iter()
            .map(|&(line, text)| (Some(line), text.to_owned()))
            .collect();
        assert_eq!(mapped_lines(source), expected);
    }

    /// the shape the bug was reported on: a trailing-lambda block's suite is
    /// hoisted into a `def` ahead of the call it hung off, and every line of
    /// that `def` used to be charged to the statement that owns the block. the
    /// generated header is that statement's, the body lines are their own, and
    /// the re-emitted call is the statement's again — through three levels of
    /// nesting and past a sibling block
    #[test]
    fn a_hoisted_block_maps_its_body_to_the_lines_it_came_from() {
        assert_mapped(
            indoc! {r#"
                def column(content: () -> None):
                    content()

                def button(label: str, on_click: () -> None):
                    on_click()

                def app():
                    column:
                        column:
                            button("a"):
                                raise ValueError("boom")
                        button("b"):
                            print("b")
                    print("after")
            "#},
            &[
                (0, "def column(content: Callable[[], None]):"),
                (1, "    content()"),
                (2, ""),
                (3, "def button(label: str, on_click: Callable[[], None]):"),
                (4, "    on_click()"),
                (5, ""),
                (6, "def app():"),
                (7, "    def _trailing_lambda_0(it=None):"),
                (8, "        def _trailing_lambda_1(it=None):"),
                (9, "            def _trailing_lambda_2(it=None):"),
                (10, "                raise ValueError(\"boom\")"),
                (9, "            button(\"a\", on_click=_trailing_lambda_2)"),
                (8, "        column(content=_trailing_lambda_1)"),
                (11, "        def _trailing_lambda_3(it=None):"),
                (12, "            print(\"b\")"),
                (11, "        button(\"b\", on_click=_trailing_lambda_3)"),
                (7, "    column(content=_trailing_lambda_0)"),
                (13, "    print(\"after\")"),
            ],
        );
    }

    /// statements between nested blocks keep their own lines on either side of
    /// the block they follow, three levels down and back up
    #[test]
    fn three_nested_blocks_each_map_to_their_own_lines() {
        assert_mapped(
            indoc! {r#"
                def run(block: () -> None):
                    block()

                def app():
                    run:
                        a = 1
                        run:
                            b = 2
                            run:
                                c = 3
                                raise ValueError("deep")
                            d = 4
                        e = 5
                    print("after")
            "#},
            &[
                (0, "def run(block: Callable[[], None]):"),
                (1, "    block()"),
                (2, ""),
                (3, "def app():"),
                (4, "    def _trailing_lambda_0(it=None):"),
                (5, "        a = 1"),
                (6, "        def _trailing_lambda_1(it=None):"),
                (7, "            b = 2"),
                (8, "            def _trailing_lambda_2(it=None):"),
                (9, "                c = 3"),
                (10, "                raise ValueError(\"deep\")"),
                (8, "            run(block=_trailing_lambda_2)"),
                (11, "            d = 4"),
                (6, "        run(block=_trailing_lambda_1)"),
                (12, "        e = 5"),
                (4, "    run(block=_trailing_lambda_0)"),
                (13, "    print(\"after\")"),
            ],
        );
    }

    /// a block inside a loop body is hoisted inside that body, and the loop
    /// header ahead of it stays its own line
    #[test]
    fn a_block_inside_a_for_loop() {
        assert_mapped(
            indoc! {"
                def button(label: str, on_click: () -> None):
                    on_click()

                def app(labels: list[str]):
                    for label in labels:
                        button(label):
                            print(label)
            "},
            &[
                (0, "def button(label: str, on_click: Callable[[], None]):"),
                (1, "    on_click()"),
                (2, ""),
                (3, "def app(labels: list[str]):"),
                (4, "    for label in labels:"),
                (5, "        def _trailing_lambda_0(it=None):"),
                (6, "            print(label)"),
                (5, "        button(label, on_click=_trailing_lambda_0)"),
            ],
        );
    }

    /// `let row = it` inside a block is a within-line rewrite of a copied line,
    /// so it stays on that line — with the trailing argument appended to a call
    /// that had no keyword yet, and to one that already had a lambda keyword
    #[test]
    fn each_and_each_indexed_blocks_binding_it() {
        assert_mapped(
            indoc! {"
                def each(items: list[int], fn: (int) -> None):
                    for item in items:
                        fn(item)

                def each_indexed(items: list[int], key: (int) -> int, fn: (int) -> None):
                    for item in items:
                        fn(key(item))

                def app(items: list[int]):
                    each(items):
                        let row = it
                        print(row)
                    each_indexed(items, key=lambda item: item + 1):
                        let row = it
                        print(row)
            "},
            &[
                (0, "def each(items: list[int], fn: Callable[[int], None]):"),
                (1, "    for item in items:"),
                (2, "        fn(item)"),
                (3, ""),
                (
                    4,
                    "def each_indexed(items: list[int], key: Callable[[int], int], fn: Callable[[int], None]):",
                ),
                (5, "    for item in items:"),
                (6, "        fn(key(item))"),
                (7, ""),
                (8, "def app(items: list[int]):"),
                (9, "    def _trailing_lambda_0(it=None):"),
                (10, "        row: Final = it"),
                (11, "        print(row)"),
                (9, "    each(items, fn=_trailing_lambda_0)"),
                (12, "    def _trailing_lambda_1(it=None):"),
                (13, "        row: Final = it"),
                (14, "        print(row)"),
                (
                    12,
                    "    each_indexed(items, key=lambda item: item + 1, fn=_trailing_lambda_1)",
                ),
            ],
        );
    }

    /// blank lines and comment lines inside the suite are copied with it and map
    /// to themselves. a comment trailing the suite's last statement is outside
    /// the span the lowering moves, so it lands after the re-emitted call, whose
    /// line is the statement's
    #[test]
    fn a_block_body_with_blank_lines_and_comments() {
        assert_mapped(
            indoc! {r#"
                def run(block: () -> None):
                    block()

                def app():
                    run:
                        # first
                        a = 1

                        # second
                        b = 2  # trailing
                    print("after")
            "#},
            &[
                (0, "def run(block: Callable[[], None]):"),
                (1, "    block()"),
                (2, ""),
                (3, "def app():"),
                (4, "    def _trailing_lambda_0(it=None):"),
                (5, "        # first"),
                (6, "        a = 1"),
                (7, ""),
                (8, "        # second"),
                (9, "        b = 2"),
                (4, "    run(block=_trailing_lambda_0)  # trailing"),
                (10, "    print(\"after\")"),
            ],
        );
    }

    /// a comment on the header line stays on the header line, which the `def`
    /// stands for
    #[test]
    fn a_header_comment_stays_on_the_header_line() {
        assert_mapped(
            indoc! {"
                def run(block: () -> None):
                    block()

                def app():
                    run:  # note
                        a = 1
                    print(a)
            "},
            &[
                (0, "def run(block: Callable[[], None]):"),
                (1, "    block()"),
                (2, ""),
                (3, "def app():"),
                (4, "    def _trailing_lambda_0(it=None):  # note"),
                (5, "        a = 1"),
                (4, "    run(block=_trailing_lambda_0)"),
                (6, "    print(a)"),
            ],
        );
    }

    /// a call header spread over several lines is re-emitted line for line, and
    /// the injected keyword — visible text of its own on the line the closing
    /// paren sat on — is the owning statement's, not that paren's line
    #[test]
    fn a_multi_line_call_header_owning_a_block() {
        assert_mapped(
            indoc! {r#"
                def button(label: str, enabled: bool, on_click: () -> None):
                    on_click()

                def app():
                    button(
                        "reset",
                        enabled=True,
                    ):
                        print("clicked")
            "#},
            &[
                (
                    0,
                    "def button(label: str, enabled: bool, on_click: Callable[[], None]):",
                ),
                (1, "    on_click()"),
                (2, ""),
                (3, "def app():"),
                (4, "    def _trailing_lambda_0(it=None):"),
                (8, "        print(\"clicked\")"),
                (4, "    button("),
                (5, "        \"reset\","),
                (6, "        enabled=True,"),
                (4, "     on_click=_trailing_lambda_0)"),
            ],
        );
    }

    /// a lambda written as an argument on the header line is copied with the
    /// call, and the block's keyword follows it
    #[test]
    fn a_lambda_argument_on_the_block_header_line() {
        assert_mapped(
            indoc! {r#"
                def field(value: str, on_change: (str) -> None, on_submit: () -> None):
                    on_change("x")
                    on_submit()

                def app():
                    field("v", on_change=lambda text: print(text)):
                        print("submitted")
            "#},
            &[
                (
                    0,
                    "def field(value: str, on_change: Callable[[str], None], on_submit: Callable[[], None]):",
                ),
                (1, "    on_change(\"x\")"),
                (2, "    on_submit()"),
                (3, ""),
                (4, "def app():"),
                (5, "    def _trailing_lambda_0(it=None):"),
                (6, "        print(\"submitted\")"),
                (
                    5,
                    "    field(\"v\", on_change=lambda text: print(text), on_submit=_trailing_lambda_0)",
                ),
            ],
        );
    }

    /// the `nonlocal` a write-through block needs is synthesized on a line of
    /// its own; nothing in the source spells it, so it is the owning statement's
    #[test]
    fn a_nonlocal_is_charged_to_the_statement_that_owns_the_block() {
        assert_mapped(
            indoc! {"
                def with_resource(once fn: (int) -> None):
                    fn(42)

                def app() -> int:
                    total: int = 1
                    with_resource:
                        total = it
                    return total
            "},
            &[
                (0, "def with_resource(fn: Callable[[int], None]):"),
                (1, "    fn(42)"),
                (2, ""),
                (3, "def app() -> int:"),
                (4, "    total: int = 1"),
                (5, "    def _trailing_lambda_0(it=None):"),
                (5, "        nonlocal total"),
                (6, "        total = it"),
                (5, "    with_resource(fn=_trailing_lambda_0)"),
                (7, "    return total"),
            ],
        );
    }

    /// everything a `once` block's `return` needs — the value cell ahead of the
    /// `def`, the pre-initialised fresh binding, the read-back after the call —
    /// is synthesized for the statement; the rewritten `return` itself keeps the
    /// returned expression, and with it its own line
    #[test]
    fn a_once_blocks_return_cell_is_charged_to_the_statement() {
        assert_mapped(
            indoc! {"
                def with_resource(once fn: (int) -> None):
                    fn(42)

                def early() -> int:
                    with_resource:
                        doubled = it * 2
                        return it + 1
                    return doubled
            "},
            &[
                (0, "def with_resource(fn: Callable[[int], None]):"),
                (1, "    fn(42)"),
                (2, ""),
                (3, "def early() -> int:"),
                (4, "    _trailing_lambda_0_return = []"),
                (4, "    doubled = None"),
                (4, "    def _trailing_lambda_0(it=None):"),
                (4, "        nonlocal doubled"),
                (5, "        doubled = it * 2"),
                (
                    6,
                    "        _trailing_lambda_0_return.append(it + 1); return",
                ),
                (4, "    with_resource(fn=_trailing_lambda_0)"),
                (4, "    if _trailing_lambda_0_return:"),
                (4, "        return _trailing_lambda_0_return[0]"),
                (7, "    return doubled"),
            ],
        );
    }

    /// a block standing as an assignment's value hoists its `def` ahead of the
    /// whole assignment, which is then re-emitted on its own line
    #[test]
    fn a_block_as_an_assignment_value() {
        assert_mapped(
            indoc! {r#"
                def totalling(fn: (int) -> None) -> str:
                    fn(3)
                    return "done"

                def app() -> str:
                    outcome = totalling:
                        print(it)
                    return outcome
            "#},
            &[
                (0, "def totalling(fn: Callable[[int], None]) -> str:"),
                (1, "    fn(3)"),
                (2, "    return \"done\""),
                (3, ""),
                (4, "def app() -> str:"),
                (5, "    def _trailing_lambda_0(it=None):"),
                (6, "        print(it)"),
                (5, "    outcome = totalling(fn=_trailing_lambda_0)"),
                (7, "    return outcome"),
            ],
        );
    }

    /// a statement an AST pass re-renders is printed from its AST, which keeps
    /// no source ranges: nothing says which rendered line came from which source
    /// line, so every line of it is charged to the statement's first line (see
    /// the re-render edit in `ast_driver::run_against_source`). the repeated
    /// `_` parameter is one such pass; the text after the statement resumes on
    /// its own lines
    #[test]
    fn a_statement_re_rendered_from_its_ast_is_charged_whole_to_its_first_line() {
        assert_mapped(
            indoc! {"
                def ignore(_: int, _: int) -> int:
                    a = 1
                    return a

                print(ignore(1, 2))
            "},
            &[
                (0, "def ignore(_: int, _2: int) -> int:"),
                (0, "    a = 1"),
                (0, "    return a"),
                (3, ""),
                (4, "print(ignore(1, 2))"),
            ],
        );
    }
}
