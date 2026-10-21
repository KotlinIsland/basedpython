//! Completing the statement at the caret, and starting the line after it.
//!
//! What an editor's *Complete Current Statement* does: the statement being typed is finished off —
//! a string literal closed, the brackets left open on the line closed, the `:` a clause header
//! needs added, a clause that continues a compound statement moved out to it — and a new line is
//! started where the next statement goes: inside the suite a header opens, or after the statement
//! at its own indentation.
//!
//! ```python
//! if handle(item, flags["x"   # completed to:
//!
//! if handle(item, flags["x"]):
//!     |
//! ```
//!
//! Which brackets are open, whether a header is missing its `:`, and where a statement ends are
//! all read off the tokens and the parse, so that a `(` in a string or a comment is not a bracket,
//! a `match = 1` is not a `match` statement, and a call spread over several lines is one statement.

use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::token::{Token, TokenKind, Tokens};
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_stmt};
use ruff_python_ast::{Stmt, StringFlags};
use ruff_python_codegen::Stylist;
use ruff_python_trivia::leading_indentation;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::Db;
use crate::on_type_formatting::{continuing_clause_reindent, header_colon, indentation_width};
use crate::syntax_outline::OutlineBuilder;

/// The statement at the caret, completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedStatement {
    /// What to change, in source order and none overlapping another, against the text as it is.
    pub edits: Vec<StatementEdit>,
    /// Where the caret goes, in the text the edits produce: the start of the new line, after its
    /// indentation.
    pub caret: TextSize,
}

/// One change: `range` replaced with `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementEdit {
    pub range: TextRange,
    pub content: String,
}

/// Completes the statement on the line `offset` is on, and starts the line after it.
///
/// `indent_unit` is one level of indentation as the file writes it, for the line a header's suite
/// starts on when there is no statement in the suite yet to copy the indentation of.
pub fn complete_statement(
    db: &dyn Db,
    file: PythonFile<'_>,
    offset: TextSize,
    indent_unit: &str,
) -> CompletedStatement {
    let parsed = parsed_module(db, file).load(db);
    let tokens = parsed.tokens();
    let source = source_text(db, file.file(db));
    let source = source.as_str();
    let line_ending = Stylist::from_tokens(tokens, source).line_ending();
    let caret_line = source.line_range(offset);

    let Some(statement) = Statement::at(tokens, caret_line) else {
        // Nothing but whitespace or a comment on the line: a new line at its indentation.
        let indentation = leading_indentation(&source[caret_line]);
        return new_line(
            source,
            caret_line.end(),
            "",
            line_ending.as_str(),
            indentation,
            Vec::new(),
        );
    };

    // The statement ends on the caret's line, unless what is open there — a bracket, a
    // triple-quoted string, a backslash — is carried on to a later line that parses. A bracket the
    // following lines do not close is not carried on: `foo(a, b` followed by `x = 1` leaves the
    // parser reading `x` as a missing comma, and it is the `(` that is missing its `)`.
    // The errors that count are the ones inside what is carried on: one where the logical line
    // ends — the `:` a header is missing — is the statement's to have completed, not a sign that
    // the lines below were never part of it.
    let carried_on = statement.logical_end > caret_line.end()
        && closes_what_is_open(tokens, statement.first, caret_line, statement.logical_end)
        && !parsed.errors().iter().any(|error| {
            error.location.start() >= caret_line.end()
                && error.location.start() < statement.logical_end
        });
    let last_line = if carried_on {
        source.line_range(statement.logical_end)
    } else {
        caret_line
    };
    let line = LineTokens::read(tokens, statement.first, last_line);

    let mut fixes = String::new();
    if !carried_on {
        for opener in line.open.iter().rev() {
            fixes.push_str(opener.closer());
        }
    }

    let mut edits = Vec::new();
    let statement_line = source.line_start(statement.start);
    let header = Header::on_line(
        db,
        file,
        tokens,
        source,
        TextRange::new(statement.start, line.code_end),
    );
    let indentation = match &header {
        Some(header) => {
            if header.needs_parentheses {
                fixes.push_str("()");
            }
            if header.needs_colon {
                fixes.push(':');
            }
            // A clause that continues a compound statement goes where that statement is.
            let reindent = (source.line_start(header.anchor) == statement_line)
                .then(|| continuing_clause_reindent(db, file, tokens, statement_line))
                .flatten();
            let header_indentation = match reindent {
                Some(reindent) => {
                    edits.push(StatementEdit {
                        range: reindent.range,
                        content: reindent.indentation.clone(),
                    });
                    reindent.indentation
                }
                None => leading_indentation(&source[source.line_range(header.anchor)]).to_string(),
            };
            match header.suite {
                // `if x: return` — the suite is done, and the next line is the statement's.
                Suite::OnTheLine => header_indentation,
                // A suite already written below is indented the way it is; a statement only
                // counts as one if it is deeper than the header, rather than a recovery's reading
                // of whatever came next.
                Suite::Below(first)
                    if indentation_width(leading_indentation(
                        &source[source.line_range(first)],
                    )) > indentation_width(&header_indentation) =>
                {
                    leading_indentation(&source[source.line_range(first)]).to_string()
                }
                Suite::Below(_) | Suite::Empty => header_indentation + indent_unit,
            }
        }
        None => leading_indentation(&source[source.line_range(statement.start)]).to_string(),
    };

    // `foo(a  # note` — the fixes go before the comment, and the new line after it.
    let new_line_at = match line.comment {
        Some(comment) => {
            if !fixes.is_empty() {
                edits.push(StatementEdit {
                    range: TextRange::empty(line.code_end),
                    content: std::mem::take(&mut fixes),
                });
            }
            comment.end()
        }
        None => line.code_end,
    };
    new_line(
        source,
        new_line_at,
        &fixes,
        line_ending.as_str(),
        &indentation,
        edits,
    )
}

/// Starts a line at `indentation` at `at`, after `prefix` and `line_ending`, dropping the
/// whitespace that ends the line there; `edits` are the other changes, all before `at`.
fn new_line(
    source: &str,
    at: TextSize,
    prefix: &str,
    line_ending: &str,
    indentation: &str,
    mut edits: Vec<StatementEdit>,
) -> CompletedStatement {
    // only whitespace follows `at` on its line: it is the end of the line's last token
    let range = TextRange::new(at, source.line_end(at));
    let content = format!("{prefix}{line_ending}{indentation}");
    let mut caret = range.start() + content.text_len();
    for edit in &edits {
        caret = caret + edit.content.text_len() - edit.range.len();
    }
    edits.push(StatementEdit { range, content });
    edits.sort_by_key(|edit| edit.range.start());
    CompletedStatement { edits, caret }
}

/// Whether the brackets left open at the end of `line` are closed before `logical_end`. A logical
/// line can end with brackets still open: at the end of the file, the parser ends it where the
/// brackets ran out of text.
fn closes_what_is_open(
    tokens: &Tokens,
    first: usize,
    line: TextRange,
    logical_end: TextSize,
) -> bool {
    let open = LineTokens::read(tokens, first, line)
        .open
        .iter()
        .filter(|opener| matches!(opener, Opener::Bracket(_)))
        .count();
    let after = tokens.partition_point(|token| token.start() < line.end());
    let depth = tokens[after..]
        .iter()
        .take_while(|token| token.start() < logical_end)
        .fold(open, |depth, token| match token.kind() {
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => depth + 1,
            TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => depth.saturating_sub(1),
            _ => depth,
        });
    depth == 0
}

/// The logical line the caret's line belongs to.
struct Statement {
    /// The index of its first token.
    first: usize,
    /// Where its first token starts.
    start: TextSize,
    /// Where the logical line ends: the start of the `Newline` token that ends it.
    logical_end: TextSize,
}

impl Statement {
    /// The statement the line `line` is part of; `None` when there is nothing on it but whitespace
    /// and comments.
    fn at(tokens: &Tokens, line: TextRange) -> Option<Self> {
        let before = tokens.partition_point(|token| token.end() <= line.start());
        let after_newline = tokens[..before]
            .iter()
            .rposition(|token| token.kind() == TokenKind::Newline)
            .map_or(0, |index| index + 1);
        let first = after_newline
            + tokens[after_newline..]
                .iter()
                .position(|token| !is_layout(token.kind()))?;
        let start = tokens[first].start();
        if start > line.end() {
            return None;
        }
        let logical_end = tokens[first..]
            .iter()
            .find(|token| token.kind() == TokenKind::Newline)
            .map_or_else(
                || tokens.last().map_or(line.end(), Ranged::end),
                Ranged::start,
            );
        Some(Self {
            first,
            start,
            logical_end,
        })
    }
}

/// What a statement's tokens say up to the end of the line the statement is completed on.
struct LineTokens {
    /// The brackets and single-quoted strings still open at the end of that line, outermost
    /// first.
    open: Vec<Opener>,
    /// The end of the line's last token that is not a comment.
    code_end: TextSize,
    /// The comment ending that line.
    comment: Option<TextRange>,
}

/// Something open that a statement being completed closes.
#[derive(Debug, Clone, Copy)]
enum Opener {
    Bracket(TokenKind),
    /// A string literal, with the quotes that close it; an f-string's or t-string's start, closed
    /// by its end, can have brackets open inside it.
    String(&'static str),
}

impl Opener {
    fn closer(self) -> &'static str {
        match self {
            Opener::Bracket(TokenKind::Lpar) => ")",
            Opener::Bracket(TokenKind::Lsqb) => "]",
            Opener::Bracket(_) => "}",
            Opener::String(quote) => quote,
        }
    }
}

impl LineTokens {
    fn read(tokens: &Tokens, first: usize, line: TextRange) -> Self {
        let mut open = Vec::new();
        let mut code_end = tokens[first].end();
        let mut comment = None;
        for token in tokens[first..]
            .iter()
            .take_while(|token| token.start() < line.end())
        {
            match token.kind() {
                TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => {
                    open.push(Opener::Bracket(token.kind()));
                }
                TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => {
                    if matches!(open.last(), Some(Opener::Bracket(_))) {
                        open.pop();
                    }
                }
                TokenKind::FStringStart | TokenKind::TStringStart => {
                    open.push(Opener::String(closing_quotes(token)));
                }
                TokenKind::FStringEnd | TokenKind::TStringEnd => {
                    // whatever was left open inside the interpolations ends with the string
                    if let Some(index) = open
                        .iter()
                        .rposition(|opener| matches!(opener, Opener::String(_)))
                    {
                        open.truncate(index);
                    }
                }
                TokenKind::String if token.start() >= line.start() => {
                    if token.string_flags().is_some_and(StringFlags::is_unclosed) {
                        open.push(Opener::String(closing_quotes(token)));
                    }
                }
                TokenKind::Comment => {
                    comment = Some(token.range());
                    continue;
                }
                kind if is_layout(kind) => continue,
                _ => {}
            }
            code_end = token.end();
        }
        Self {
            open,
            code_end,
            comment: comment.filter(|comment| comment.start() >= code_end),
        }
    }
}

/// The quotes that close the string `token` starts. A triple-quoted string left open runs on to
/// the end of the file rather than to the end of the line, so where it was meant to end is not for
/// the line to say; it is left open.
fn closing_quotes(token: &Token) -> &'static str {
    match token.string_flags() {
        Some(flags) if !flags.is_triple_quoted() => flags.quote_str(),
        _ => "",
    }
}

/// Tokens that are part of no statement: line breaks, indentation and comments.
fn is_layout(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Newline
            | TokenKind::NonLogicalNewline
            | TokenKind::Indent
            | TokenKind::Dedent
            | TokenKind::Comment
    )
}

/// A clause header on the logical line being completed.
struct Header {
    /// Where the header's keyword is, or its start for one with none (a trailing lambda's).
    anchor: TextSize,
    needs_colon: bool,
    /// A `def` written without its parameter list.
    needs_parentheses: bool,
    suite: Suite,
}

/// Where a header's suite is.
enum Suite {
    /// There is none yet.
    Empty,
    /// On the header's own line: `if x: return`.
    OnTheLine,
    /// Starting at this offset, on a later line.
    Below(TextSize),
}

impl Header {
    /// The clause header the code of the logical line in `line` is, if it is one.
    fn on_line(
        db: &dyn Db,
        file: PythonFile<'_>,
        tokens: &Tokens,
        source: &str,
        line: TextRange,
    ) -> Option<Self> {
        let parsed = parsed_module(db, file).load(db);
        let mut finder = HeaderFinder {
            builder: OutlineBuilder::new(source, tokens),
            tokens,
            line,
            found: None,
        };
        finder.visit_body(parsed.suite());
        if let Some(header) = finder.found {
            return Some(header);
        }

        // A clause keyword the parser made no statement of — an `else` with no `if` above it — is
        // still a header being written.
        let first = tokens.partition_point(|token| token.start() < line.start());
        let keyword = tokens.get(first)?;
        let is_compound_keyword = matches!(
            keyword.kind(),
            TokenKind::If
                | TokenKind::Elif
                | TokenKind::Else
                | TokenKind::While
                | TokenKind::For
                | TokenKind::With
                | TokenKind::Try
                | TokenKind::Except
                | TokenKind::Finally
                | TokenKind::Def
                | TokenKind::Class
                | TokenKind::Async
                // a soft keyword the parser kept as one
                | TokenKind::Match
                | TokenKind::Case
        );
        if !is_compound_keyword {
            return None;
        }
        let has_colon =
            header_colon(&tokens[first + 1..]).is_some_and(|colon| colon.start() < line.end());
        Some(Self {
            anchor: keyword.start(),
            needs_colon: !has_colon,
            needs_parentheses: false,
            suite: Suite::Empty,
        })
    }
}

/// Finds the first clause header whose keyword is on the line being completed, statement
/// expressions' included.
struct HeaderFinder<'a> {
    builder: OutlineBuilder<'a>,
    tokens: &'a Tokens,
    line: TextRange,
    found: Option<Header>,
}

impl<'a> SourceOrderVisitor<'a> for HeaderFinder<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.found.is_some() || stmt.range().intersect(self.line).is_none() {
            return;
        }
        for (index, head) in self.builder.clause_heads(stmt).iter().enumerate() {
            // the `let` half of a `let ... := ... else:` opens no suite of its own
            if index == 0 && matches!(stmt, Stmt::Let(_)) {
                continue;
            }
            let anchor = head.keyword.map_or(head.range.start(), TextRange::start);
            if !self.line.contains_inclusive(anchor) {
                continue;
            }
            let needs_parentheses = match stmt {
                Stmt::FunctionDef(function) if !function.is_trailing_lambda && index == 0 => {
                    let from = self
                        .tokens
                        .partition_point(|token| token.start() < function.name.end());
                    !self.tokens[from..]
                        .iter()
                        .take_while(|token| token.start() < self.line.end())
                        .any(|token| token.kind() == TokenKind::Lpar)
                }
                _ => false,
            };
            let suite = match head.body.first() {
                None => Suite::Empty,
                Some(first) if first.start() <= self.line.end() => Suite::OnTheLine,
                Some(first) => Suite::Below(first.start()),
            };
            self.found = Some(Header {
                anchor,
                needs_colon: head.colon.is_none(),
                needs_parentheses,
                suite,
            });
            return;
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use ruff_source_file::LineEnding;

    use super::complete_statement;
    use crate::tests::CursorTest;

    /// The source after completing the statement at `<CURSOR>`, with `<CURSOR>` where the caret
    /// goes.
    fn complete_in(path: &str, source: &str) -> String {
        let test = CursorTest::builder().source(path, source).build();
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        let completed = complete_statement(&test.db, file, test.cursor.offset, "    ");
        let mut text = test.cursor.source.as_str().to_string();
        for edit in completed.edits.iter().rev() {
            text.replace_range(edit.range.to_std_range(), &edit.content);
        }
        text.insert_str(completed.caret.to_usize(), "<CURSOR>");
        text
    }

    fn complete(source: &str) -> String {
        complete_in("main.by", source)
    }

    #[test]
    fn a_header_gets_its_colon_and_a_suite() {
        assert_eq!(complete("if x<CURSOR>\n"), "if x:\n    <CURSOR>\n");
        assert_eq!(
            complete("for item in items<CURSOR>\n"),
            "for item in items:\n    <CURSOR>\n"
        );
        assert_eq!(complete("while<CURSOR> x\n"), "while x:\n    <CURSOR>\n");
        assert_eq!(complete("class A<CURSOR>\n"), "class A:\n    <CURSOR>\n");
        assert_eq!(complete("try<CURSOR>\n"), "try:\n    <CURSOR>\n");
        assert_eq!(
            complete("with open(p) as f<CURSOR>\n"),
            "with open(p) as f:\n    <CURSOR>\n"
        );
    }

    #[test]
    fn a_def_gets_its_parameters_too() {
        assert_eq!(complete("def f<CURSOR>\n"), "def f():\n    <CURSOR>\n");
        assert_eq!(complete("def f(x<CURSOR>\n"), "def f(x):\n    <CURSOR>\n");
        assert_eq!(
            complete("def f(x) -> int<CURSOR>\n"),
            "def f(x) -> int:\n    <CURSOR>\n"
        );
    }

    #[test]
    fn open_brackets_are_closed_innermost_first() {
        assert_eq!(
            complete("foo(a, [1, (2<CURSOR>\n"),
            "foo(a, [1, (2)])\n<CURSOR>\n"
        );
        assert_eq!(
            complete("if handle(item, flags[\"x\"<CURSOR>\n"),
            "if handle(item, flags[\"x\"]):\n    <CURSOR>\n"
        );
    }

    #[test]
    fn a_bracket_the_next_line_does_not_close_is_closed() {
        assert_eq!(
            complete("foo(a, b<CURSOR>\nx = 1\n"),
            "foo(a, b)\n<CURSOR>\nx = 1\n"
        );
    }

    #[test]
    fn a_bracket_open_at_the_end_of_the_file_is_closed() {
        assert_eq!(complete("foo(a,<CURSOR>\n"), "foo(a,)\n<CURSOR>\n");
    }

    #[test]
    fn a_statement_carried_on_below_is_completed_where_it_ends() {
        assert_eq!(
            complete("foo(a,<CURSOR>\n    b)\nx = 1\n"),
            "foo(a,\n    b)\n<CURSOR>\nx = 1\n"
        );
        assert_eq!(
            complete("if (a and<CURSOR>\n        b)\n"),
            "if (a and\n        b):\n    <CURSOR>\n"
        );
    }

    #[test]
    fn a_string_left_open_is_closed() {
        assert_eq!(
            complete("print(\"hello<CURSOR>\n"),
            "print(\"hello\")\n<CURSOR>\n"
        );
        assert_eq!(complete("x = 'a<CURSOR>\n"), "x = 'a'\n<CURSOR>\n");
        assert_eq!(
            complete("x = f\"a {y<CURSOR>\n"),
            "x = f\"a {y}\"\n<CURSOR>\n"
        );
    }

    #[test]
    fn a_comment_stays_at_the_end() {
        assert_eq!(
            complete("foo(a<CURSOR>  # note\n"),
            "foo(a)  # note\n<CURSOR>\n"
        );
        assert_eq!(
            complete("if x  # why<CURSOR>\n"),
            "if x:  # why\n    <CURSOR>\n"
        );
    }

    #[test]
    fn a_complete_statement_gets_a_new_line_at_its_indentation() {
        assert_eq!(
            complete("def f():\n    x = g(<CURSOR>1)\n    return x\n"),
            "def f():\n    x = g(1)\n    <CURSOR>\n    return x\n"
        );
    }

    #[test]
    fn a_header_with_a_suite_below_starts_a_line_at_the_suites_indentation() {
        assert_eq!(
            complete("if x<CURSOR>:\n  pass\n"),
            "if x:\n  <CURSOR>\n  pass\n"
        );
    }

    #[test]
    fn a_one_line_suite_is_left_done() {
        assert_eq!(
            complete("def f():\n    if x: return<CURSOR> 1\n"),
            "def f():\n    if x: return 1\n    <CURSOR>\n"
        );
    }

    #[test]
    fn a_continuing_clause_is_moved_out_to_its_statement() {
        assert_eq!(
            complete("if x:\n    a()\n    else<CURSOR>\n"),
            "if x:\n    a()\nelse:\n    <CURSOR>\n"
        );
        assert_eq!(
            complete("try:\n    a()\n    except ValueError<CURSOR>\n"),
            "try:\n    a()\nexcept ValueError:\n    <CURSOR>\n"
        );
    }

    /// A clause with no statement above it to continue is read off its tokens, where a `:` in
    /// brackets is not the one ending the header.
    #[test]
    fn a_colon_in_brackets_does_not_end_a_clause_the_parser_made_nothing_of() {
        assert_eq!(
            complete("if a:\n    pass\n    elif d[1:2]<CURSOR>\n"),
            "if a:\n    pass\nelif d[1:2]:\n    <CURSOR>\n"
        );
        assert_eq!(
            complete("elif {1: 2}<CURSOR>\n"),
            "elif {1: 2}:\n    <CURSOR>\n"
        );
    }

    /// The line a statement is completed with ends the way the file's lines do.
    #[test]
    fn a_new_line_ends_like_the_file_s_lines() {
        assert_eq!(complete("x = 1<CURSOR>\r\n"), "x = 1\r\n<CURSOR>\r\n");
        assert_eq!(
            complete("if x<CURSOR>\r\ny = 2\r\n"),
            "if x:\r\n    <CURSOR>\r\ny = 2\r\n"
        );
    }

    #[test]
    fn soft_keywords_are_the_parsers_call() {
        assert_eq!(complete("match x<CURSOR>\n"), "match x:\n    <CURSOR>\n");
        assert_eq!(complete("match = 1<CURSOR>\n"), "match = 1\n<CURSOR>\n");
    }

    #[test]
    fn an_if_statement_expression_is_a_header() {
        assert_eq!(
            complete("label = if n == 0<CURSOR>\n"),
            "label = if n == 0:\n    <CURSOR>\n"
        );
    }

    #[test]
    fn a_blank_line_gets_a_new_line() {
        assert_eq!(
            complete("def f():\n    <CURSOR>\n"),
            "def f():\n    \n    <CURSOR>\n"
        );
    }

    #[test]
    fn trailing_whitespace_does_not_stay_behind() {
        assert_eq!(complete("x = 1<CURSOR>   \n"), "x = 1\n<CURSOR>\n");
    }

    /// A file with no line ending to copy gets the platform's, as every other edit does.
    #[test]
    fn the_last_line_of_a_file_without_a_newline() {
        assert_eq!(
            complete("foo(1<CURSOR>"),
            format!("foo(1){}<CURSOR>", LineEnding::default().as_str())
        );
    }

    #[test]
    fn python_files_are_completed_too() {
        assert_eq!(
            complete_in("main.py", "elif_ = 1\nif x<CURSOR>\n"),
            "elif_ = 1\nif x:\n    <CURSOR>\n"
        );
    }
}
