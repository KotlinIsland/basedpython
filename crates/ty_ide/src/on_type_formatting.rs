//! What typing a character changes about the line it was typed on.
//!
//! A clause that continues a compound statement — `elif`, `else`, `except`, `finally`, a further
//! `case` — is written at the indentation of the clause it continues. An editor starts a new line
//! at the indentation of the one before, which is the body the clause closes, so the clause is
//! typed a level or more too deep. Typing its `:` is the moment to move it out: the header is
//! whole by then, and which statement it continues is decided by what is above it.
//!
//! ```python
//! for item in items:
//!     if item:
//!         handle(item)
//!         else:   # typed here; moved out to the `if` once the `:` is typed
//! ```
//!
//! The statement it continues is the innermost one that is still open above the line, can take
//! that clause next, and is not indented deeper than the line already is: a clause is only ever
//! moved out, so one written at the indentation of an outer statement stays with that statement.

use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::token::{Token, TokenKind, Tokens};
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_parser::{ParseOptions, parse_unchecked};
use ruff_python_trivia::leading_indentation;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::Db;

/// A line's indentation, replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reindent {
    /// The line's leading whitespace as it is.
    pub range: TextRange,
    /// What replaces it: the leading whitespace of the line the clause lines up with, as written
    /// there, so a file indented with tabs stays indented with tabs.
    pub indentation: String,
}

/// What typing `typed`, which ends at `offset`, changes about its line: the indentation of a
/// clause whose header that character finished, when the clause is not yet lined up with the
/// statement it continues.
pub fn on_type_formatting(
    db: &dyn Db,
    file: PythonFile<'_>,
    offset: TextSize,
    typed: &str,
) -> Option<Reindent> {
    if typed != ":" {
        return None;
    }
    let parsed = parsed_module(db, file).load(db);
    let tokens = parsed.tokens();
    let start = tokens.partition_point(|token| token.end() < offset);
    let colon = tokens[start..]
        .iter()
        .take_while(|token| token.start() < offset)
        .find(|token| token.kind() == TokenKind::Colon && token.end() == offset)?;
    clause_reindent(db, file, tokens, colon.range())
}

/// The indentation the clause whose header ends at the `:` at `colon` belongs at, when that is
/// not where it is.
fn clause_reindent(
    db: &dyn Db,
    file: PythonFile<'_>,
    tokens: &Tokens,
    colon: TextRange,
) -> Option<Reindent> {
    let source = source_text(db, file.file(db));
    let line_start = source.line_start(colon.start());
    let (index, keyword) = continuing_keyword(source.as_str(), tokens, line_start)?;
    if !colon_ends_header(tokens, index, keyword, colon) {
        return None;
    }
    line_reindent(db, file, line_start, keyword)
}

/// The indentation the line starting at `line_start` belongs at, when it starts a clause that
/// continues a compound statement and is not lined up with that statement yet.
///
/// Whether the clause's header is whole is the caller's to have made sure of.
pub(crate) fn continuing_clause_reindent(
    db: &dyn Db,
    file: PythonFile<'_>,
    tokens: &Tokens,
    line_start: TextSize,
) -> Option<Reindent> {
    let source = source_text(db, file.file(db));
    let (_, keyword) = continuing_keyword(source.as_str(), tokens, line_start)?;
    line_reindent(db, file, line_start, keyword)
}

fn line_reindent(
    db: &dyn Db,
    file: PythonFile<'_>,
    line_start: TextSize,
    keyword: ContinuingKeyword,
) -> Option<Reindent> {
    let source = source_text(db, file.file(db));
    let source = source.as_str();
    let indentation = leading_indentation(&source[line_start.to_usize()..]);

    // The statements above the line are read from the text above it alone: the clause itself,
    // written where it does not belong yet, is an error in the full parse, and what the parser
    // makes of the statements around an error is a recovery rather than the structure the clause
    // is being lined up with.
    let above = parse_unchecked(&source[..line_start.to_usize()], parse_options(db, file));
    let ast::Mod::Module(module) = above.syntax() else {
        return None;
    };
    let mut open = Vec::new();
    open_statements(&module.body, &mut open);

    let width = indentation_width(indentation);
    let anchor = open.iter().rev().find_map(|statement| {
        let anchor = continued_at(statement, keyword)?;
        let anchor_line = source.line_start(anchor);
        let anchor_indentation = leading_indentation(&source[TextRange::new(anchor_line, anchor)]);
        (indentation_width(anchor_indentation) <= width).then_some(anchor_indentation)
    })?;
    (anchor != indentation).then(|| Reindent {
        range: TextRange::at(line_start, indentation.text_len()),
        indentation: anchor.to_string(),
    })
}

/// The options `file` is parsed with, for parsing part of its text the way the whole of it is
/// parsed: basedpython syntax in a basedpython file, at the Python version the file is checked
/// against.
fn parse_options(db: &dyn Db, file: PythonFile<'_>) -> ParseOptions {
    ParseOptions::from(file.file(db).source_type(db)).with_target_version(file.python_version(db))
}

/// A keyword that starts a clause continuing a compound statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuingKeyword {
    Elif,
    Else,
    Except,
    Finally,
    Case,
}

/// The token index and kind of the keyword the line starting at `line_start` opens with, when it
/// is one that continues a compound statement and it starts a logical line — rather than
/// continuing one inside brackets or after a backslash.
fn continuing_keyword(
    source: &str,
    tokens: &Tokens,
    line_start: TextSize,
) -> Option<(usize, ContinuingKeyword)> {
    let keyword_start =
        line_start + leading_indentation(&source[line_start.to_usize()..]).text_len();
    // past the zero-width `Dedent`s that start where a dedented line's first token does
    let first = tokens.partition_point(|token| token.start() < keyword_start);
    let first = first
        + tokens[first..]
            .iter()
            .take_while(|token| token.kind() == TokenKind::Dedent)
            .count();
    let keyword_token = tokens.get(first)?;
    if keyword_token.start() != keyword_start {
        return None;
    }
    let keyword = match keyword_token.kind() {
        TokenKind::Elif => ContinuingKeyword::Elif,
        TokenKind::Else => ContinuingKeyword::Else,
        TokenKind::Except => ContinuingKeyword::Except,
        TokenKind::Finally => ContinuingKeyword::Finally,
        // `case` is a soft keyword, and the parser only keeps a `case` token for one inside a
        // `match`; written in a body where it does not belong yet, it reads as a name.
        TokenKind::Case => ContinuingKeyword::Case,
        TokenKind::Name if &source[keyword_token.range()] == "case" => ContinuingKeyword::Case,
        _ => return None,
    };
    let starts_line = tokens[..first]
        .iter()
        .rev()
        .find(|token| {
            !matches!(
                token.kind(),
                TokenKind::Indent
                    | TokenKind::Dedent
                    | TokenKind::Comment
                    | TokenKind::NonLogicalNewline
            )
        })
        .is_none_or(|token| token.kind() == TokenKind::Newline);
    starts_line.then_some((first, keyword))
}

/// Whether the `:` at `colon` ends the header that `keyword`, the token at `index`, starts —
/// rather than belonging to a lambda in it, or sitting in brackets.
fn colon_ends_header(
    tokens: &Tokens,
    index: usize,
    keyword: ContinuingKeyword,
    colon: TextRange,
) -> bool {
    if header_colon(&tokens[index + 1..]) != Some(colon) {
        return false;
    }
    let between = &tokens[index + 1..tokens.partition_point(|token| token.start() < colon.start())];
    let has_header = between.iter().any(|token| !token.kind().is_trivia());
    match keyword {
        // nothing comes between these and their `:`
        ContinuingKeyword::Else | ContinuingKeyword::Finally => !has_header,
        // a `case` needs a pattern; `case: int = 1` annotates a variable called `case`
        ContinuingKeyword::Case => has_header,
        ContinuingKeyword::Elif | ContinuingKeyword::Except => true,
    }
}

/// The `:` that ends the clause header `tokens` start after its keyword: the first one outside
/// brackets and not a lambda's, before the logical line ends.
pub(crate) fn header_colon(tokens: &[Token]) -> Option<TextRange> {
    let mut depth = 0u32;
    let mut lambdas = 0u32;
    for token in tokens {
        match token.kind() {
            TokenKind::Newline => return None,
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => depth += 1,
            TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => {
                depth = depth.saturating_sub(1);
            }
            TokenKind::Lambda if depth == 0 => lambdas += 1,
            TokenKind::Colon if depth == 0 => match lambdas.checked_sub(1) {
                Some(rest) => lambdas = rest,
                None => return Some(token.range()),
            },
            _ => {}
        }
    }
    None
}

/// The statements a line after `suite` could still continue, outermost first: the last statement
/// of the suite, the last statement of that one's last clause, and so on down.
fn open_statements<'a>(suite: &'a [Stmt], open: &mut Vec<&'a Stmt>) {
    let Some(last) = suite.last() else {
        return;
    };
    // A basedpython statement expression — `label = if count == 0:` — is a compound statement
    // that ends where the simple statement holding it does, and continues like one.
    let statement = trailing_statement_expression(last).unwrap_or(last);
    open.push(statement);
    if let Some(body) = last_suite(statement) {
        open_statements(body, open);
    }
}

/// The suite of `statement`'s last clause, for a compound statement.
fn last_suite(statement: &Stmt) -> Option<&[Stmt]> {
    let suite: &[Stmt] = match statement {
        Stmt::If(if_stmt) => if_stmt
            .elif_else_clauses
            .last()
            .map_or(&if_stmt.body, |clause| &clause.body),
        Stmt::For(ast::StmtFor { body, orelse, .. })
        | Stmt::While(ast::StmtWhile { body, orelse, .. }) => {
            if orelse.is_empty() {
                body
            } else {
                orelse
            }
        }
        Stmt::Try(try_stmt) => [&try_stmt.finalbody, &try_stmt.orelse]
            .into_iter()
            .find(|suite| !suite.is_empty())
            .map_or_else(
                || {
                    try_stmt.handlers.last().map_or(&try_stmt.body, |handler| {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        &handler.body
                    })
                },
                |suite| suite,
            ),
        Stmt::With(with_stmt) => &with_stmt.body,
        Stmt::FunctionDef(function) => &function.body,
        Stmt::ClassDef(class) => &class.body,
        Stmt::Match(match_stmt) => &match_stmt.cases.last()?.body,
        Stmt::Let(let_stmt) => &let_stmt.orelse,
        _ => return None,
    };
    Some(suite)
}

/// Where the line `keyword` would continue `statement` lines up, when `statement` can take that
/// clause next: the start of the statement's first keyword, or, for a `case`, of its last `case`.
fn continued_at(statement: &Stmt, keyword: ContinuingKeyword) -> Option<TextSize> {
    match (statement, keyword) {
        (Stmt::If(if_stmt), ContinuingKeyword::Elif | ContinuingKeyword::Else) => if_stmt
            .elif_else_clauses
            .last()
            .is_none_or(|clause| clause.test.is_some())
            .then(|| if_stmt.start()),
        (
            Stmt::For(ast::StmtFor { orelse, .. }) | Stmt::While(ast::StmtWhile { orelse, .. }),
            ContinuingKeyword::Else,
        ) => orelse.is_empty().then(|| statement.start()),
        (Stmt::Try(try_stmt), ContinuingKeyword::Except) => {
            (try_stmt.orelse.is_empty() && try_stmt.finalbody.is_empty()).then(|| try_stmt.start())
        }
        (Stmt::Try(try_stmt), ContinuingKeyword::Else) => (!try_stmt.handlers.is_empty()
            && try_stmt.orelse.is_empty()
            && try_stmt.finalbody.is_empty())
        .then(|| try_stmt.start()),
        (Stmt::Try(try_stmt), ContinuingKeyword::Finally) => {
            try_stmt.finalbody.is_empty().then(|| try_stmt.start())
        }
        (Stmt::Match(match_stmt), ContinuingKeyword::Case) => {
            match_stmt.cases.last().map(Ranged::start)
        }
        _ => None,
    }
}

/// The compound statement a simple statement ends with, when it ends with a statement expression.
fn trailing_statement_expression(statement: &Stmt) -> Option<&Stmt> {
    struct Finder<'a> {
        end: TextSize,
        found: Option<&'a Stmt>,
    }

    impl<'a> SourceOrderVisitor<'a> for Finder<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            // Only an expression ending where the statement does can hold the one it ends with,
            // and the outermost such statement expression is the one a clause would continue.
            if self.found.is_some() || expr.end() != self.end {
                return;
            }
            if let Expr::Statement(statement) = expr {
                self.found = Some(statement.stmt.as_ref());
            } else {
                walk_expr(self, expr);
            }
        }
    }

    if !is_simple(statement) {
        return None;
    }
    let mut finder = Finder {
        end: statement.end(),
        found: None,
    };
    walk_stmt(&mut finder, statement);
    finder.found
}

fn is_simple(statement: &Stmt) -> bool {
    !matches!(
        statement,
        Stmt::If(_)
            | Stmt::For(_)
            | Stmt::While(_)
            | Stmt::Try(_)
            | Stmt::With(_)
            | Stmt::FunctionDef(_)
            | Stmt::ClassDef(_)
            | Stmt::Match(_)
            | Stmt::Let(_)
    )
}

/// How many columns `indentation` spans, a tab reaching the next multiple of eight as it does for
/// Python's tokenizer.
pub(crate) fn indentation_width(indentation: &str) -> usize {
    indentation.chars().fold(0, |column, char| match char {
        '\t' => (column / 8 + 1) * 8,
        _ => column + 1,
    })
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextSize;

    use super::on_type_formatting;
    use crate::tests::CursorTest;

    /// The source after typing a `:` at `<CURSOR>`: the `:` is inserted, and the reindent applied.
    fn typed_colon(path: &str, source: &str) -> String {
        let source = source.replace("<CURSOR>", ":<CURSOR>");
        let test = CursorTest::builder().source(path, &source).build();
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        let text = test.cursor.source.as_str().to_string();
        match on_type_formatting(&test.db, file, test.cursor.offset, ":") {
            Some(reindent) => {
                let mut text = text;
                text.replace_range(reindent.range.to_std_range(), &reindent.indentation);
                text
            }
            None => text,
        }
    }

    fn by(source: &str) -> String {
        typed_colon("main.by", source)
    }

    #[test]
    fn else_moves_out_to_its_if() {
        assert_eq!(
            by("if x:\n    a = 1\n    else<CURSOR>\n"),
            "if x:\n    a = 1\nelse:\n"
        );
    }

    #[test]
    fn elif_moves_out_to_its_if() {
        assert_eq!(
            by("if x:\n    a = 1\n    elif y<CURSOR>\n"),
            "if x:\n    a = 1\nelif y:\n"
        );
    }

    #[test]
    fn else_goes_to_the_innermost_if_not_deeper_than_it() {
        let source = "for i in r:\n    if x:\n        pass\n        else<CURSOR>\n";
        assert_eq!(
            by(source),
            "for i in r:\n    if x:\n        pass\n    else:\n"
        );
    }

    #[test]
    fn a_clause_already_at_an_outer_statement_stays_there() {
        let source = "for i in r:\n    if x:\n        pass\nelse<CURSOR>\n";
        assert_eq!(by(source), source.replace("<CURSOR>", ":"));
    }

    #[test]
    fn else_skips_an_if_that_has_one() {
        let source =
            "for i in r:\n    if x:\n        pass\n    else:\n        pass\n    else<CURSOR>\n";
        // the `if` already has its `else`; the next one out that can take one is the loop
        assert_eq!(
            by(source),
            "for i in r:\n    if x:\n        pass\n    else:\n        pass\nelse:\n"
        );
    }

    #[test]
    fn except_and_finally_move_out_to_their_try() {
        assert_eq!(
            by("try:\n    a()\n    except ValueError as e<CURSOR>\n"),
            "try:\n    a()\nexcept ValueError as e:\n"
        );
        assert_eq!(
            by("try:\n    a()\nexcept E:\n    b()\n    finally<CURSOR>\n"),
            "try:\n    a()\nexcept E:\n    b()\nfinally:\n"
        );
    }

    #[test]
    fn else_after_a_try_needs_an_except_first() {
        let source = "try:\n    a()\n    else<CURSOR>\n";
        assert_eq!(by(source), source.replace("<CURSOR>", ":"));
    }

    #[test]
    fn case_lines_up_with_the_case_before() {
        assert_eq!(
            by("match x:\n    case 1:\n        a()\n        case 2<CURSOR>\n"),
            "match x:\n    case 1:\n        a()\n    case 2:\n"
        );
    }

    #[test]
    fn a_first_case_is_left_where_it_is() {
        let source = "match x:\n        case 1<CURSOR>\n";
        assert_eq!(by(source), source.replace("<CURSOR>", ":"));
    }

    #[test]
    fn a_variable_called_case_is_not_a_clause() {
        let source = "match x:\n    case 1:\n        case<CURSOR> int = 1\n";
        assert_eq!(by(source), source.replace("<CURSOR>", ":"));
    }

    #[test]
    fn a_colon_that_does_not_end_the_header_changes_nothing() {
        for source in [
            "if x:\n    pass\n    elif y[1<CURSOR>]\n",
            "if x:\n    pass\n    elif lambda<CURSOR>\n",
            "if x:\n    pass\n    elif {1<CURSOR> 2}\n",
        ] {
            assert_eq!(by(source), source.replace("<CURSOR>", ":"), "{source:?}");
        }
    }

    #[test]
    fn a_lambdas_colon_is_passed_over() {
        assert_eq!(
            by("if x:\n    pass\n    elif lambda: y<CURSOR>\n"),
            "if x:\n    pass\nelif lambda: y:\n"
        );
    }

    #[test]
    fn a_keyword_inside_brackets_is_not_a_clause() {
        let source = "if x:\n    f(\n        else<CURSOR>\n";
        assert_eq!(by(source), source.replace("<CURSOR>", ":"));
    }

    #[test]
    fn a_line_after_a_simple_statement_at_its_level_is_left_alone() {
        let source = "if x:\n    pass\ny = 1\n    else<CURSOR>\n";
        assert_eq!(by(source), source.replace("<CURSOR>", ":"));
    }

    #[test]
    fn an_if_statement_expression_takes_its_else() {
        assert_eq!(
            by("label = if n == 0:\n    \"none\"\n    else<CURSOR>\n"),
            "label = if n == 0:\n    \"none\"\nelse:\n"
        );
    }

    #[test]
    fn tabs_are_kept() {
        assert_eq!(
            by("def f():\n\tif x:\n\t\tpass\n\t\telse<CURSOR>\n"),
            "def f():\n\tif x:\n\t\tpass\n\telse:\n"
        );
    }

    #[test]
    fn python_files_are_reindented_too() {
        assert_eq!(
            typed_colon("main.py", "while x:\n    pass\n    else<CURSOR>\n"),
            "while x:\n    pass\nelse:\n"
        );
    }

    #[test]
    fn only_a_colon_triggers() {
        let test = CursorTest::builder()
            .source("main.by", "if x:\n    pass\n    else:<CURSOR>\n")
            .build();
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        assert_eq!(
            on_type_formatting(&test.db, file, test.cursor.offset, "\n"),
            None
        );
        assert_eq!(
            on_type_formatting(&test.db, file, test.cursor.offset - TextSize::new(1), ":"),
            None
        );
    }
}
