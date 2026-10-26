//! a `by repl` session: basedpython typed an input at a time, run in one python process
//!
//! a session is a module whose source is the inputs that ran, in order, and each new input is
//! checked and transpiled as that module's continuation (see `docs/basedpython/features/repl.md`).
//! What is here is the part of that which is a question about source text, so the command and
//! the language server give the same answers: whether what has been typed is a whole input yet.

use ruff_python_ast::{PySourceType, Stmt};
use ruff_python_parser::{
    InterpolatedStringErrorType, LexicalErrorType, ParseError, ParseErrorType,
    parse_unchecked_source,
};

/// whether the text typed so far is an input that can run
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputStatus {
    /// it can run as it is
    Complete,
    /// it needs more lines: a compound statement whose body could still go on, an open
    /// bracket, an unterminated triple-quoted string, a line continued with `\`
    Incomplete,
    /// no line that follows can make it valid. running it reports why
    Invalid,
}

/// whether `text`, the lines typed so far, is a whole input, as python's repl decides it for
/// python: a statement runs once a line completes it, a compound statement once a blank line
/// follows it, since until then its body could go on, and input that no further line can make
/// valid runs at once, to report what is wrong with it
///
/// blank input, and input of comments only, is complete: there is nothing to wait for
pub fn input_status(text: &str) -> InputStatus {
    let parsed = parse_unchecked_source(text, PySourceType::BasedPython);
    // the parser goes on past an error and can report others that follow from it, so the
    // first one is the one that says what went wrong
    let Some(first) = parsed
        .errors()
        .iter()
        .min_by_key(|error| error.location.start())
    else {
        return match parsed.suite().last() {
            Some(last) if has_suite(last) && !ends_with_blank_line(text) => InputStatus::Incomplete,
            _ => InputStatus::Complete,
        };
    };
    if ran_out_inside_a_token(text, first) {
        return InputStatus::Incomplete;
    }
    // the parser met the end of the input where the grammar needs more: a block's body, a
    // decorator's definition, a `try`'s handler. a blank line says the input has ended, as
    // it does after a compound statement
    if parsed.ran_out_of_input() && !ends_with_blank_line(text) {
        InputStatus::Incomplete
    } else {
        InputStatus::Invalid
    }
}

/// whether the input ended while the tokenizer was inside something only a later line can
/// close: an open bracket or a line continued with `\`, which it reports as running into the
/// end of the input, or a triple-quoted string, which it reports where the string starts
fn ran_out_inside_a_token(text: &str, error: &ParseError) -> bool {
    match &error.error {
        ParseErrorType::Lexical(
            LexicalErrorType::Eof
            | LexicalErrorType::FStringError(
                InterpolatedStringErrorType::UnterminatedTripleQuotedString,
            )
            | LexicalErrorType::TStringError(
                InterpolatedStringErrorType::UnterminatedTripleQuotedString,
            ),
        ) => true,
        ParseErrorType::Lexical(LexicalErrorType::UnclosedStringError) => {
            opens_triple_quoted_string(&text[usize::from(error.location.start())..])
        }
        _ => false,
    }
}

/// whether `text` begins with a string literal whose quotes are tripled, after any prefix
fn opens_triple_quoted_string(text: &str) -> bool {
    let quotes = text.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    quotes.starts_with("\"\"\"") || quotes.starts_with("'''")
}

/// whether `stmt` ends in an indented suite, which a line typed after it could still extend.
/// a bodiless `def`, basedpython's overload declaration, is one line
fn has_suite(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::FunctionDef(def) => !def.body.is_empty(),
        Stmt::ClassDef(class) => !class.body.is_empty(),
        Stmt::For(_)
        | Stmt::While(_)
        | Stmt::If(_)
        | Stmt::With(_)
        | Stmt::Match(_)
        | Stmt::Try(_) => true,
        // `let [first, *rest] := words else:` and its block
        Stmt::Let(binding) => !binding.orelse.is_empty(),
        // `type Element[T] = match T:` and its `case` blocks
        Stmt::TypeAlias(alias) => !alias.cases.is_empty(),
        _ => false,
    }
}

/// whether the last line of `text` is blank: a line ending the input that holds nothing but
/// whitespace, which is what ends a compound statement typed a line at a time
fn ends_with_blank_line(text: &str) -> bool {
    let Some(before_last_newline) = text.strip_suffix('\n') else {
        return false;
    };
    let before_last_newline = before_last_newline
        .strip_suffix('\r')
        .unwrap_or(before_last_newline);
    let last_line = before_last_newline
        .rsplit_once('\n')
        .map_or(before_last_newline, |(_, line)| line);
    last_line.trim().is_empty() && before_last_newline.contains('\n')
}

#[cfg(test)]
mod tests {
    use super::{InputStatus, input_status};

    #[track_caller]
    fn assert_status(text: &str, expected: InputStatus) {
        assert_eq!(input_status(text), expected, "{text:?}");
    }

    #[test]
    fn a_simple_statement_is_complete_once_typed() {
        assert_status("x = 1\n", InputStatus::Complete);
        assert_status("x = 1", InputStatus::Complete);
        assert_status("print(x)\n", InputStatus::Complete);
        assert_status("let rate = 0.2\n", InputStatus::Complete);
        assert_status("x ?? 1\n", InputStatus::Complete);
    }

    #[test]
    fn nothing_typed_is_complete() {
        assert_status("", InputStatus::Complete);
        assert_status("\n", InputStatus::Complete);
        assert_status("   \n", InputStatus::Complete);
        assert_status("# a comment\n", InputStatus::Complete);
    }

    /// as in python's repl, a compound statement runs at the blank line after it, since until
    /// then another line of its body, or an `else`, could follow
    #[test]
    fn a_compound_statement_waits_for_a_blank_line() {
        assert_status("if x:\n", InputStatus::Incomplete);
        assert_status("if x:\n    y = 1\n", InputStatus::Incomplete);
        assert_status("if x:\n    y = 1\n\n", InputStatus::Complete);
        assert_status("if x:\n    y = 1\n    \n", InputStatus::Complete);
        assert_status("def f():\n    return 1\n", InputStatus::Incomplete);
        assert_status("def f():\n    return 1\n\n", InputStatus::Complete);
        assert_status("def f(): return 1\n", InputStatus::Incomplete);
        assert_status("class C:\n    x: int\n", InputStatus::Incomplete);
        assert_status("for i in xs:\n    pass\n", InputStatus::Incomplete);
        assert_status("while x:\n    pass\n", InputStatus::Incomplete);
        assert_status("with f() as g:\n    pass\n", InputStatus::Incomplete);
        assert_status(
            "try:\n    pass\nexcept E:\n    pass\n",
            InputStatus::Incomplete,
        );
        assert_status(
            "match x:\n    case 1:\n        pass\n",
            InputStatus::Incomplete,
        );
        assert_status("@decorator\n", InputStatus::Incomplete);
        assert_status("@decorator\ndef f():\n    pass\n", InputStatus::Incomplete);
    }

    /// a header waits for the block that follows it, whatever the statement: a line of its own
    /// could still start that block
    #[test]
    fn every_compound_header_waits_for_its_body() {
        for header in [
            "if x:",
            "while x:",
            "for i in xs:",
            "async for i in xs:",
            "with f() as g:",
            "async with f() as g:",
            "def f():",
            "def f() -> int:",
            "async def f():",
            "class C:",
            "class C(Base):",
            "try:",
            "match x:",
            "if let x := y:",
            "let [a, b] := xs else:",
            "type Element[T] = match T:",
        ] {
            assert_status(&format!("{header}\n"), InputStatus::Incomplete);
            assert_status(header, InputStatus::Incomplete);
        }
    }

    #[test]
    fn a_continuation_clause_waits_for_its_body() {
        assert_status("if x:\n    pass\nelif y:\n", InputStatus::Incomplete);
        assert_status("if x:\n    pass\nelse:\n", InputStatus::Incomplete);
        assert_status("for i in xs:\n    pass\nelse:\n", InputStatus::Incomplete);
        assert_status("while x:\n    pass\nelse:\n", InputStatus::Incomplete);
        assert_status("try:\n    pass\nexcept E:\n", InputStatus::Incomplete);
        assert_status("try:\n    pass\nexcept* E:\n", InputStatus::Incomplete);
        assert_status(
            "try:\n    pass\nexcept E:\n    pass\nelse:\n",
            InputStatus::Incomplete,
        );
        assert_status("try:\n    pass\nfinally:\n", InputStatus::Incomplete);
        assert_status("match x:\n    case 1:\n", InputStatus::Incomplete);
    }

    /// a `try` is not over until a handler or `finally` follows its block, so the next line
    /// could still be one
    #[test]
    fn a_try_block_waits_for_its_handler() {
        assert_status("try:\n    pass\n", InputStatus::Incomplete);
        assert_status("try:\n    x = 1\n    y = 2\n", InputStatus::Incomplete);
        assert_status("try:\n    pass\n\n", InputStatus::Invalid);
    }

    #[test]
    fn a_decorator_waits_for_what_it_decorates() {
        assert_status("@decorator\n", InputStatus::Incomplete);
        assert_status("@decorator\n@other\n", InputStatus::Incomplete);
        assert_status("class C:\n    @property\n", InputStatus::Incomplete);
        assert_status(
            "def f():\n    class C:\n        @property\n",
            InputStatus::Incomplete,
        );
        assert_status("@decorator\n\n", InputStatus::Invalid);
    }

    /// as in python's repl, a blank line ends the input, so a header with no body after it
    /// cannot be finished any more
    #[test]
    fn a_blank_line_after_a_header_is_invalid() {
        assert_status("if x:\n\n", InputStatus::Invalid);
        assert_status("try:\n\n", InputStatus::Invalid);
        assert_status("match x:\n\n", InputStatus::Invalid);
        assert_status("if x:\n    pass\nelse:\n\n", InputStatus::Invalid);
    }

    #[test]
    fn a_comment_line_does_not_end_a_compound_statement() {
        assert_status("if x:\n    y = 1\n# more\n", InputStatus::Incomplete);
    }

    /// a statement that only spans several lines, because a bracket was open, is complete
    /// once the bracket closes, as python's repl runs it
    #[test]
    fn a_simple_statement_over_several_lines_is_complete_when_it_closes() {
        assert_status("x = (1,\n", InputStatus::Incomplete);
        assert_status("x = (1,\n     2)\n", InputStatus::Complete);
        assert_status("call(\n", InputStatus::Incomplete);
        assert_status("xs = [\n    1,\n", InputStatus::Incomplete);
        assert_status("x = 1 + \\\n", InputStatus::Incomplete);
        assert_status("x = 1 + \\\n    2\n", InputStatus::Complete);
    }

    #[test]
    fn an_open_triple_quoted_string_is_incomplete() {
        assert_status("x = \"\"\"one\n", InputStatus::Incomplete);
        assert_status("x = '''one\ntwo\n", InputStatus::Incomplete);
        assert_status("x = f\"\"\"one {y}\n", InputStatus::Incomplete);
        assert_status("x = \"\"\"one\n\"\"\"\n", InputStatus::Complete);
    }

    #[test]
    fn an_error_no_line_can_fix_is_invalid() {
        assert_status("x = = 1\n", InputStatus::Invalid);
        assert_status("x = \"one\n", InputStatus::Invalid);
        assert_status(")\n", InputStatus::Invalid);
        assert_status("  x = 1\n", InputStatus::Invalid);
        assert_status("else:\n", InputStatus::Invalid);
        assert_status("if x:\ny = 1\n", InputStatus::Invalid);
        assert_status("x = 1 +\n", InputStatus::Invalid);
        assert_status("lambda:\n", InputStatus::Invalid);
        assert_status("def f(:\n", InputStatus::Invalid);
        assert_status("x = (1, = 2\n", InputStatus::Invalid);
        assert_status("if x:\n    pass\nelse\n", InputStatus::Invalid);
        assert_status("x = f\"a{\n", InputStatus::Invalid);
        assert_status("x = 1 +", InputStatus::Invalid);
        assert_status("x = = (1,\n", InputStatus::Invalid);
        assert_status("if x:\n    @decorator\ny = 1\n", InputStatus::Invalid);
        assert_status("try:\n    pass\nx = 1\n", InputStatus::Invalid);
    }

    #[test]
    fn a_nested_block_header_waits_for_its_body() {
        assert_status("def f():\n    if x:\n", InputStatus::Incomplete);
        assert_status("class C:\n    @property\n", InputStatus::Incomplete);
        assert_status("def f():\n\tif x:\n", InputStatus::Incomplete);
        assert_status("if x:\n    pass\nelif y:\n", InputStatus::Incomplete);
        assert_status("@dec(1)\n# why\n", InputStatus::Incomplete);
    }

    /// a framed input is sent whole, so a compound statement followed by a simple one is one
    /// input that ends in a simple statement
    #[test]
    fn statements_after_a_compound_one_complete_the_input() {
        assert_status("if x:\n    y = 1\nz = 2\n", InputStatus::Complete);
        assert_status(
            "def f():\n    a = 1\n\n    return a\nf()\n",
            InputStatus::Complete,
        );
    }

    #[test]
    fn basedpython_compound_statements_wait_for_a_blank_line() {
        assert_status("if let x := y:\n    pass\n", InputStatus::Incomplete);
        assert_status(
            "let [a, b] := xs else:\n    raise E\n",
            InputStatus::Incomplete,
        );
        assert_status(
            "let [a, b] := xs else:\n    raise E\n\n",
            InputStatus::Complete,
        );
    }
}
