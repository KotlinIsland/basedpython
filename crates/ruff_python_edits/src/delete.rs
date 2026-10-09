//! deleting a statement without leaving the source around it invalid

use ruff_diagnostics::Edit;
use ruff_python_ast::{self as ast, ExceptHandler, Stmt};
use ruff_python_index::Indexer;
use ruff_python_trivia::{PythonWhitespace, has_leading_content, is_python_whitespace};
use ruff_source_file::{LineRanges, NewlineWithTrailingNewline};
use ruff_text_size::{Ranged, TextLen, TextSize};

/// Return the [`Edit`] to use when deleting a [`Stmt`].
///
/// In some cases, this is as simple as deleting the
/// [`TextRange`](ruff_text_size::TextRange) of the [`Stmt`] itself. However, there are a
/// few exceptions:
/// - If the [`Stmt`] is _not_ the terminal statement in a multi-statement line,
///   we need to delete up to the start of the next statement (and avoid
///   deleting any content that precedes the statement).
/// - If the [`Stmt`] is the terminal statement in a multi-statement line, we need
///   to avoid deleting any content that precedes the statement.
/// - If the [`Stmt`] has no trailing and leading content, then it's convenient to
///   remove the entire start and end lines.
/// - If the [`Stmt`] is the last statement in its parent body, replace it with a
///   `pass` instead.
pub fn delete_stmt(stmt: &Stmt, parent: Option<&Stmt>, source: &str, indexer: &Indexer) -> Edit {
    if parent.is_some_and(|parent| is_lone_child(stmt, parent)) {
        // If removing this node would lead to an invalid syntax tree, replace
        // it with a `pass`.
        Edit::range_replacement("pass".to_string(), stmt.range())
    } else {
        if let Some(semicolon) = trailing_semicolon(stmt.end(), source) {
            let next = next_stmt_break(semicolon, source);
            Edit::deletion(stmt.start(), next)
        } else if has_leading_content(stmt.start(), source) {
            Edit::range_deletion(stmt.range())
        } else if let Some(start) = indexer.preceded_by_continuations(stmt.start(), source) {
            Edit::deletion(start, stmt.end())
        } else {
            let range = source.full_lines_range(stmt.range());
            Edit::range_deletion(range)
        }
    }
}

/// Determine if a vector contains only one, specific element.
fn is_only<T: PartialEq>(vec: &[T], value: &T) -> bool {
    vec.len() == 1 && vec[0] == *value
}

/// Determine if a child is the only statement in its body.
fn is_lone_child(child: &Stmt, parent: &Stmt) -> bool {
    match parent {
        Stmt::FunctionDef(ast::StmtFunctionDef { body, .. })
        | Stmt::ClassDef(ast::StmtClassDef { body, .. })
        | Stmt::With(ast::StmtWith { body, .. })
            if is_only(body, child) =>
        {
            return true;
        }
        Stmt::For(ast::StmtFor { body, orelse, .. })
        | Stmt::While(ast::StmtWhile { body, orelse, .. })
            if (is_only(body, child) || is_only(orelse, child)) =>
        {
            return true;
        }
        Stmt::If(ast::StmtIf {
            body,
            elif_else_clauses,
            ..
        }) if (is_only(body, child)
            || elif_else_clauses
                .iter()
                .any(|ast::ElifElseClause { body, .. }| is_only(body, child))) =>
        {
            return true;
        }
        Stmt::Try(ast::StmtTry {
            body,
            handlers,
            orelse,
            finalbody,
            ..
        }) if (is_only(body, child)
            || is_only(orelse, child)
            || is_only(finalbody, child)
            || handlers.iter().any(|handler| match handler {
                ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler { body, .. }) => {
                    is_only(body, child)
                }
            })) =>
        {
            return true;
        }
        Stmt::Match(ast::StmtMatch { cases, .. })
            if cases.iter().any(|case| is_only(&case.body, child)) =>
        {
            return true;
        }
        _ => {}
    }
    false
}

/// Return the location of a trailing semicolon following a `Stmt`, if it's part
/// of a multi-statement line.
fn trailing_semicolon(offset: TextSize, source: &str) -> Option<TextSize> {
    let contents = &source[offset.to_usize()..];

    for line in NewlineWithTrailingNewline::from(contents) {
        let trimmed = line.trim_whitespace_start();

        if trimmed.starts_with(';') {
            let colon_offset = line.text_len() - trimmed.text_len();
            return Some(offset + line.start() + colon_offset);
        }

        if !trimmed.starts_with('\\') {
            break;
        }
    }
    None
}

/// Find the next valid break for a `Stmt` after a semicolon.
fn next_stmt_break(semicolon: TextSize, source: &str) -> TextSize {
    let start_location = semicolon + TextSize::from(1);

    for line in NewlineWithTrailingNewline::with_offset(
        &source[start_location.to_usize()..],
        start_location,
    ) {
        let trimmed = line.trim_whitespace();
        // Skip past any continuations.
        if trimmed.starts_with('\\') {
            continue;
        }

        return if trimmed.is_empty() {
            // If the line is empty, then despite the previous statement ending in a
            // semicolon, we know that it's not a multi-statement line.
            line.start()
        } else {
            // Otherwise, find the start of the next statement. (Or, anything that isn't
            // whitespace.)
            let relative_offset = line.find(|c: char| !is_python_whitespace(c)).unwrap();
            line.start() + TextSize::try_from(relative_offset).unwrap()
        };
    }

    source.line_end(start_location)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use ruff_python_ast::Stmt;
    use ruff_python_parser::parse_module;
    use ruff_text_size::{Ranged, TextSize};

    use super::{next_stmt_break, trailing_semicolon};

    /// Parse the given source using [`Mode::Module`] and return the first statement.
    fn parse_first_stmt(source: &str) -> Result<Stmt> {
        let suite = parse_module(source)?.into_suite();
        Ok(suite.into_iter().next().unwrap())
    }

    #[test]
    fn find_semicolon() -> Result<()> {
        let contents = "x = 1";
        let stmt = parse_first_stmt(contents)?;
        assert_eq!(trailing_semicolon(stmt.end(), contents), None);

        let contents = "x = 1; y = 1";
        let stmt = parse_first_stmt(contents)?;
        assert_eq!(
            trailing_semicolon(stmt.end(), contents),
            Some(TextSize::from(5))
        );

        let contents = "x = 1 ; y = 1";
        let stmt = parse_first_stmt(contents)?;
        assert_eq!(
            trailing_semicolon(stmt.end(), contents),
            Some(TextSize::from(6))
        );

        let contents = r"
x = 1 \
  ; y = 1
"
        .trim();
        let stmt = parse_first_stmt(contents)?;
        assert_eq!(
            trailing_semicolon(stmt.end(), contents),
            Some(TextSize::from(10))
        );

        Ok(())
    }

    #[test]
    fn find_next_stmt_break() {
        let contents = "x = 1; y = 1";
        assert_eq!(
            next_stmt_break(TextSize::from(4), contents),
            TextSize::from(5)
        );

        let contents = "x = 1 ; y = 1";
        assert_eq!(
            next_stmt_break(TextSize::from(5), contents),
            TextSize::from(6)
        );

        let contents = r"
x = 1 \
  ; y = 1
"
        .trim();
        assert_eq!(
            next_stmt_break(TextSize::from(10), contents),
            TextSize::from(12)
        );
    }
}
