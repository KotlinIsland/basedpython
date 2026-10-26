//! Helpers for adding suppression comments without changing which existing suppression is used.
//!
//! An applicable same-line or nested own-line suppression is extended first because it has the
//! narrowest scope. For diagnostics spanning multiple lines, an opening-line suppression takes
//! precedence over a separate closing-line suppression, matching normal suppression resolution.
//! Comments with trailing reasons are never extended: preserving the reason requires adding a
//! separate suppression instead.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Formatter;

use ruff_db::PythonFile;
use ruff_db::diagnostic::LintName;
use ruff_db::display::FormatterJoinExtension;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::token::TokenKind;
use ruff_python_trivia::encoding_declaration;
use ruff_source_file::UniversalNewlineIterator;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};
use smallvec::SmallVec;

use crate::Db;
use crate::lint::LintId;
use crate::suppression::{
    SuppressionKind, Suppressions, select_preferred_suppression, suppressions,
};

/// Creates fixes to suppress all violations in `ids_with_range`.
///
/// Unlike calling [`suppress_single`] for each diagnostic, this groups diagnostics that can share
/// an edit. It appends codes once to each applicable existing suppression and otherwise inserts at
/// most one end-of-line suppression at each destination. Every returned [`SuppressFix`] records
/// how many diagnostics its edit accounts for.
pub(crate) fn suppress_all(
    db: &dyn Db,
    file: PythonFile<'_>,
    ids_with_range: &[(LintName, TextRange)],
) -> Vec<SuppressFix> {
    let suppressions = suppressions(db, file);
    let source = source_text(db, file.file(db));
    let parsed = parsed_module(db, file).load(db);
    let tokens = parsed.tokens();

    // Compute the full suppression ranges for each diagnostic, while retaining the original
    // diagnostic ranges for matching existing suppressions.
    let mut ids_with_suppression_range: Vec<_> = ids_with_range
        .iter()
        .map(|&(id, diagnostic_range)| {
            (
                id,
                diagnostic_range,
                suppression_range(db, file, diagnostic_range),
            )
        })
        .collect();

    // Sort the suppression ranges by their start position and length (end position).
    // This ensures that a diagnostic with a shorter range is processed before
    // a diagnostic starting on the same line, but with a wider range (ends on a later line).
    //
    // ```
    // diag["home_assistant"]["entities"] = sorted(
    // ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ wider range
    // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~ narrower range
    //     diag["home_assistant"]["entities"], key=lambda ent: ent["entity_id"]
    // )  # end of the wider range
    // ^ wider range
    // ```
    //
    // This is important because a suppression inserted at the end of a narrower range
    // can result in a start-line suppression for a wider range. In the example above,
    // inserting a `ty: ignore` after `sorted(` suppresses the diagnostic with the narrower range
    // but also the diagnostic with the wider range (because the suppression is on its start line).
    ids_with_suppression_range.sort_unstable_by_key(|(_, _, range)| (range.start(), range.end()));

    let mut fixes = Vec::with_capacity(ids_with_suppression_range.len());
    let mut with_existing = Vec::new();
    let mut without_existing = Vec::new();

    // Choose the final existing suppression for every diagnostic before grouping any edits.
    for (id, diagnostic_range, suppression_range) in ids_with_suppression_range {
        if let Some(existing) = find_existing_suppression(suppressions, &source, diagnostic_range) {
            with_existing.push((id, suppression_range, existing));
        } else {
            without_existing.push((id, suppression_range));
        }
    }

    // Tracks newly inserted end-of-line suppressions by the physical line where they become start
    // suppressions. This avoids inserting another suppression for a wider same-code diagnostic
    // that starts on that line (see the example above).
    let mut by_line = BTreeMap::<TextSize, BTreeMap<LintName, TextSize>>::new();
    let mut by_end: BTreeMap<TextSize, (BTreeSet<LintName>, usize)> = BTreeMap::new();

    for (id, range) in without_existing {
        let existing_end = by_line
            .get(&range.start())
            .and_then(|planned| planned.get(&id))
            .copied();

        if let Some(end_offset) = existing_end {
            let (_, suppressed_diagnostics) = by_end.entry(end_offset).or_default();
            *suppressed_diagnostics += 1;
            continue;
        }

        let (lints, suppressed_diagnostics) = by_end.entry(range.end()).or_default();
        lints.insert(id);
        *suppressed_diagnostics += 1;

        by_line
            .entry(line_start(tokens, range.end()))
            .or_default()
            .entry(id)
            .or_insert(range.end());
    }

    let mut by_suppression =
        BTreeMap::<TextSize, (ExistingSuppression, BTreeSet<LintName>, usize)>::new();

    // Reconcile existing-comment edits after planning new suppressions. A new suppression inserted
    // at the end of a narrower range can cover the start of a wider diagnostic and make an edit to
    // the wider diagnostic's existing end-line suppression immediately unused.
    for (id, range, existing) in with_existing {
        if let Some(end_offset) = by_line
            .get(&range.start())
            .and_then(|planned| planned.get(&id))
        {
            let (_, suppressed_diagnostics) = by_end.entry(*end_offset).or_default();
            *suppressed_diagnostics += 1;
            continue;
        }

        let insertion_offset = existing.insertion_offset;
        let (_, grouped_codes, grouped_diagnostics) = by_suppression
            .entry(insertion_offset)
            .or_insert_with(|| (existing, BTreeSet::new(), 0));
        grouped_codes.insert(id);
        *grouped_diagnostics += 1;
    }

    for (end_offset, (lints, suppressed_diagnostics)) in by_end {
        let codes: SmallVec<[LintName; 2]> = lints.into_iter().collect();
        fixes.push(SuppressFix {
            fix: add_end_of_line_suppression(&source, &codes, end_offset),
            suppressed_diagnostics,
        });
    }

    for (existing, codes, suppressed_diagnostics) in by_suppression.into_values() {
        let codes: SmallVec<[LintName; 2]> = codes.into_iter().collect();
        fixes.push(SuppressFix {
            fix: add_to_existing_suppression(existing, &codes),
            suppressed_diagnostics,
        });
    }

    fixes
}

/// Fix to suppress one or more diagnostics.
pub(crate) struct SuppressFix {
    pub(crate) fix: Fix,
    /// The number of diagnostics that will be suppressed if this fix is applied.
    pub(crate) suppressed_diagnostics: usize,
}

/// Creates a fix to suppress a single lint.
pub fn suppress_single(db: &dyn Db, file: PythonFile<'_>, id: LintId, range: TextRange) -> Fix {
    let suppression_range = suppression_range(db, file, range);

    let suppressions = suppressions(db, file);
    let source = source_text(db, file.file(db));
    let codes = &[id.name()];

    if let Some(existing) = find_existing_suppression(suppressions, &source, range) {
        return add_to_existing_suppression(existing, codes);
    }

    add_end_of_line_suppression(&source, codes, suppression_range.end())
}

/// creates a fix to suppress a lint in the whole of `file`
///
/// that is a `ty: ignore` comment on a line of its own at the top of the file, before any code,
/// which the typing spec reads as covering the whole file. an editable one there already takes
/// the code instead of a second comment being written; a new one goes on the first line, or
/// after a `#!` line or an encoding declaration, which have to stay where they are
pub fn suppress_in_file(db: &dyn Db, file: PythonFile<'_>, id: LintId) -> Fix {
    let suppressions = suppressions(db, file);
    let source = source_text(db, file.file(db));
    let codes = &[id.name()];

    let existing = suppressions.file.iter().find_map(|suppression| {
        let prefix = editable_suppression_prefix(&source[suppression.comment_range])?;
        let separator = if prefix.ends_with('[') { "" } else { ", " };
        Some(ExistingSuppression {
            insertion_offset: suppression.comment_range.start() + prefix.text_len(),
            kind: suppression.kind,
            separator,
        })
    });
    if let Some(existing) = existing {
        return add_to_existing_suppression(existing, codes);
    }

    let offset = file_suppression_offset(&source);
    // the line the comment goes after needs a newline ending it, which the last line of a file
    // may not have
    let before = source[..offset.to_usize()].trim_start_matches('\u{feff}');
    let leading = if before.is_empty() || before.ends_with(['\n', '\r']) {
        ""
    } else {
        "\n"
    };
    Fix::safe_edit(Edit::insertion(
        format!(
            "{leading}# ty: ignore[{codes}]\n",
            codes = Codes(SuppressionKind::Ty, codes)
        ),
        offset,
    ))
}

/// where a new file-level suppression goes: after the lines python reads before any code, which
/// have to stay where they are. that is a `#!` line, which has to be the first, and a PEP 263
/// encoding declaration, which python reads only on the first two lines. a byte order mark
/// stays ahead of everything
fn file_suppression_offset(source: &str) -> TextSize {
    let start = if source.starts_with('\u{feff}') {
        '\u{feff}'.text_len()
    } else {
        TextSize::ZERO
    };
    if let Some(declaration) = encoding_declaration(source.as_bytes()) {
        return declaration.line.end();
    }
    match UniversalNewlineIterator::with_offset(&source[start.to_usize()..], start).next() {
        Some(first) if first.starts_with("#!") => first.full_end(),
        _ => start,
    }
}

/// Returns the suppression range for the given `range`.
///
/// The suppression range is defined as:
///
/// * `start`: The `end` of the preceding `Newline` or `NonLogicalLine` token.
/// * `end`: The `start` of the first `NonLogicalLine` or `Newline` token coming after the range.
///
/// For most ranges, this means the suppression range starts at the beginning of the physical line
/// and ends at the end of the physical line containing `range`. The exceptions to this are:
///
/// * If `range` is within a single-line interpolated expression, then the start and end are extended to the start and end of the enclosing interpolated string.
/// * If there's a line continuation, then the suppression range is extended to include the following line too.
/// * If there's a multiline string, then the suppression range is extended to cover the starting and ending line of the multiline string.
fn suppression_range(db: &dyn Db, file: PythonFile<'_>, range: TextRange) -> TextRange {
    // Always insert a new suppression at the end of the range to avoid having to deal with multiline strings
    // etc. Also make sure to not pass a sub-token range to `Tokens::after`.
    let parsed = parsed_module(db, file).load(db);
    let line_start = line_start(parsed.tokens(), range.start());

    let after_token_range = match parsed.tokens().at_offset(range.end()) {
        ruff_python_ast::token::TokenAt::None => range,
        ruff_python_ast::token::TokenAt::Single(token) => token.range(),
        ruff_python_ast::token::TokenAt::Between(..) => range,
    };
    let after_tokens = parsed.tokens().after(after_token_range.end());
    let line_end = after_tokens
        .iter()
        .find(|token| {
            matches!(
                token.kind(),
                TokenKind::Newline | TokenKind::NonLogicalNewline
            )
        })
        .map(Ranged::start)
        .unwrap_or(range.end());

    TextRange::new(line_start, line_end)
}

fn line_start(tokens: &ruff_python_ast::token::Tokens, offset: TextSize) -> TextSize {
    tokens
        .before(tokens.token_range(offset).start())
        .iter()
        .rfind(|token| {
            matches!(
                token.kind(),
                TokenKind::Newline | TokenKind::NonLogicalNewline
            )
        })
        .map(Ranged::end)
        .unwrap_or_default()
}

fn add_end_of_line_suppression(source: &str, codes: &[LintName], line_end: TextSize) -> Fix {
    let up_to_line_end = &source[..line_end.to_usize()];
    // Don't use `trim_end` in case the previous line ends with a `\` followed by a newline. We don't want to eat
    // into that newline!
    let up_to_first_content =
        up_to_line_end.trim_end_matches(|c| !matches!(c, '\n' | '\r') && c.is_whitespace());
    let trailing_whitespace_len = up_to_line_end.text_len() - up_to_first_content.text_len();

    let insertion = format!(
        "  # ty: ignore[{codes}]",
        codes = Codes(SuppressionKind::Ty, codes)
    );

    Fix::safe_edit(if trailing_whitespace_len == TextSize::ZERO {
        Edit::insertion(insertion, line_end)
    } else {
        // `expr # fmt: off<trailing_whitespace>`
        // Trim the trailing whitespace
        Edit::replacement(insertion, line_end - trailing_whitespace_len, line_end)
    })
}

/// Returns insertion metadata for the preferred editable suppression covering `range`.
///
/// When multiple comments apply, a same-line or otherwise nested comment takes precedence over an
/// outer own-line suppression.
///
/// ```python
/// # ty: ignore[invalid-assignment]
/// values: tuple[int] = [missing]  # ty: ignore[]
/// ```
fn find_existing_suppression(
    suppressions: &Suppressions,
    source: &str,
    range: TextRange,
) -> Option<ExistingSuppression> {
    let suppression = select_preferred_suppression(
        suppressions
            .editable_inline_suppressions_rev(range)
            .filter(|suppression| {
                editable_suppression_prefix(&source[suppression.comment_range]).is_some()
            }),
        range,
    )?;
    let prefix = editable_suppression_prefix(&source[suppression.comment_range])?;
    let separator = if prefix.ends_with('[') {
        ""
    } else if prefix.ends_with(',') {
        " "
    } else {
        ", "
    };

    Some(ExistingSuppression {
        insertion_offset: suppression.comment_range.start() + prefix.text_len(),
        kind: suppression.kind,
        separator,
    })
}

fn add_to_existing_suppression(existing: ExistingSuppression, codes: &[LintName]) -> Fix {
    let separator = existing.separator;
    let insertion = format!("{separator}{codes}", codes = Codes(existing.kind, codes));

    Fix::safe_edit(Edit::insertion(insertion, existing.insertion_offset))
}

/// Returns the portion of an ignore comment before its closing bracket if another code can be
/// appended to it.
///
/// ```python
/// # ty: ignore[]         # Editable
/// # ty: ignore[] reason  # Not editable
/// ```
fn editable_suppression_prefix(comment_text: &str) -> Option<&str> {
    // The parser accepts a reason after the code list, but rule codes can't contain `]`, so the
    // first `]` is the code list's closing bracket. Don't edit comments with trailing reasons.
    let (before_closing_bracket, after_closing_bracket) = comment_text.split_once(']')?;
    after_closing_bracket
        .trim()
        .is_empty()
        .then(|| before_closing_bracket.trim_end())
}

#[derive(Copy, Clone)]
struct ExistingSuppression {
    insertion_offset: TextSize,
    kind: SuppressionKind,
    separator: &'static str,
}

struct Codes<'a>(SuppressionKind, &'a [LintName]);

impl std::fmt::Display for Codes<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut joiner = f.join(", ");

        let namespace = if self.0.is_type_ignore() { "ty:" } else { "" };

        for item in self.1 {
            joiner.entry(&format_args!("{namespace}{item}"));
        }

        joiner.finish()
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ruff_db::source::source_text;
    use ruff_db::{PythonFile, system::DbWithWritableSystem as _};
    use ruff_text_size::Ranged as _;

    use super::suppress_in_file;
    use crate::Db as _;
    use crate::db::tests::TestDbBuilder;
    use crate::suppression::suppressions;

    /// `source` with a file-level suppression of `unresolved-reference` added, checking that the
    /// comment it adds is one that covers the whole file
    fn suppressed_in_file(source: &str) -> String {
        let mut db = TestDbBuilder::new()
            .with_file("test.py", source)
            .build()
            .unwrap();
        let file = system_path_to_file(&db, "test.py").unwrap();
        let lint = db.lint_registry().get("unresolved-reference").unwrap();
        let fix = suppress_in_file(&db, PythonFile::new(&db, file, db.python_version()), lint);
        let mut suppressed = source.to_owned();
        for edit in fix.edits().iter().rev() {
            suppressed.replace_range(
                edit.start().to_usize()..edit.end().to_usize(),
                edit.content().unwrap_or_default(),
            );
        }
        db.write_file("test.py", &suppressed).unwrap();
        let file = system_path_to_file(&db, "test.py").unwrap();
        assert_eq!(source_text(&db, file).as_str(), suppressed);
        let python_file = PythonFile::new(&db, file, db.python_version());
        assert_eq!(
            suppressions(&db, python_file).file.len(),
            1,
            "{suppressed:?}"
        );
        suppressed
    }

    #[test]
    fn file_suppression_goes_first() {
        assert_eq!(
            suppressed_in_file("a = b\n"),
            "# ty: ignore[unresolved-reference]\na = b\n"
        );
    }

    #[test]
    fn file_suppression_goes_after_an_encoding_declaration() {
        assert_eq!(
            suppressed_in_file("# -*- coding: latin-1 -*-\na = b\n"),
            "# -*- coding: latin-1 -*-\n# ty: ignore[unresolved-reference]\na = b\n"
        );
        assert_eq!(
            suppressed_in_file("#!/usr/bin/env python\n# vim: set fileencoding=utf-8 :\na = b\n"),
            "#!/usr/bin/env python\n# vim: set fileencoding=utf-8 :\n\
             # ty: ignore[unresolved-reference]\na = b\n"
        );
        assert_eq!(
            suppressed_in_file("# a comment\n# coding=utf-8\na = b\n"),
            "# a comment\n# coding=utf-8\n# ty: ignore[unresolved-reference]\na = b\n"
        );
    }

    /// python reads a declaration on the second line only when the first could not be code, and
    /// a comment that only mentions an encoding is not one
    #[test]
    fn file_suppression_goes_first_without_an_encoding_declaration() {
        assert_eq!(
            suppressed_in_file("# no coding here\na = b\n"),
            "# ty: ignore[unresolved-reference]\n# no coding here\na = b\n"
        );
        assert_eq!(
            suppressed_in_file("a = b\n# coding: utf-8\n"),
            "# ty: ignore[unresolved-reference]\na = b\n# coding: utf-8\n"
        );
    }

    #[test]
    fn file_suppression_goes_after_a_byte_order_mark() {
        assert_eq!(
            suppressed_in_file("\u{feff}a = b\n"),
            "\u{feff}# ty: ignore[unresolved-reference]\na = b\n"
        );
        assert_eq!(
            suppressed_in_file("\u{feff}# coding: utf-8\r\na = b\r\n"),
            "\u{feff}# coding: utf-8\r\n# ty: ignore[unresolved-reference]\na = b\r\n"
        );
    }

    #[test]
    fn file_suppression_after_a_last_line_without_a_newline() {
        assert_eq!(
            suppressed_in_file("#!/usr/bin/env python"),
            "#!/usr/bin/env python\n# ty: ignore[unresolved-reference]\n"
        );
    }
}
