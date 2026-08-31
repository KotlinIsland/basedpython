/// Build an output-line → input-line table for a single edit-application pass.
///
/// `edits` must be ascending by start and non-overlapping, expressed in `source`
/// byte coordinates (the same shape `replace_range` is fed). Each output *line*
/// maps to the input line it came from. This is the line-level primitive the
/// run-time traceback rewriter composes; column-accurate mapping is future work
/// (see `docs/basedpython/development/sourcemaps.md`).
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
pub fn line_table(source: &str, edits: &[(usize, usize, String)]) -> Vec<Option<u32>> {
    let mut lines: Vec<Option<u32>> = Vec::new();
    let mut src_pos = 0usize;
    let mut input_line = 0u32;
    // the input line behind the output line being built: set once a character
    // has landed on it, cleared by the `\n` that completes it. what it holds at
    // the end is the entry for a last line nothing terminated
    let mut open: Option<u32> = None;

    for (start, end, new_text) in edits {
        for ch in source[src_pos..*start].chars() {
            if ch == '\n' {
                lines.push(Some(input_line));
                input_line += 1;
                open = None;
            } else {
                open = Some(input_line);
            }
        }
        let consumed = source[*start..*end].chars().filter(|&c| c == '\n').count();
        // a replacement's lines are all attributed to the line the edit starts
        // on: it is the one `.by` line the reader can be pointed at, whatever
        // shape the generated text took
        for ch in new_text.chars() {
            if ch == '\n' {
                lines.push(Some(input_line));
                open = None;
            } else {
                open = Some(input_line);
            }
        }
        input_line += u32::try_from(consumed).unwrap_or(0);
        src_pos = *end;
    }
    for ch in source[src_pos..].chars() {
        if ch == '\n' {
            lines.push(Some(input_line));
            input_line += 1;
            open = None;
        } else {
            open = Some(input_line);
        }
    }
    if let Some(origin) = open {
        lines.push(Some(origin));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::line_table;

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
        let widened = line_table("a = 1\nb = 2", &[(10, 11, "22".to_owned())]);
        assert_eq!(widened, vec![Some(0), Some(1)]);

        // a replacement that brings its own line break: both generated lines
        // are attributed to the `.by` line the edit started on
        let split = line_table("a = 1\nb = 2", &[(6, 11, "b = 2\nc = 3".to_owned())]);
        assert_eq!(split, vec![Some(0), Some(1), Some(1)]);
    }
}
