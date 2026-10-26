//! a `by repl` session: basedpython typed an input at a time, run in one python process
//!
//! a session is a module whose source is the inputs that ran, in order, and each new input is
//! checked and transpiled as that module's continuation (see `docs/basedpython/features/repl.md`).
//! What is here is the part of that which is a question about source text: whether what has been
//! typed is a whole input yet, which the command and the language server both answer, and which
//! of the python the module comes to is the new input's to run.

use std::collections::HashSet;

use ruff_python_ast::{PySourceType, Stmt};
use ruff_python_parser::{
    InterpolatedStringErrorType, LexicalErrorType, ParseError, ParseErrorType,
    parse_unchecked_source,
};
use ruff_text_size::{Ranged, TextSize};

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

/// The inputs of a session that ran, as the source of the one module they make.
///
/// Each new input is checked and transpiled as this module's continuation, by
/// [`Session::with_input`]; [`Session::select`] then picks out of the python the module came
/// to the statements that are the input's to run, and [`Session::commit`] makes the input part
/// of the session once it is handed over to run.
#[derive(Debug, Default)]
pub struct Session {
    source: String,
    /// each input that ran, in order, with the line of `source` it starts at
    inputs: Vec<RanInput>,
    /// the python each statement that has run was transpiled to. a statement is matched by
    /// its text, which is what it means to python: one transpiled the same way again is the
    /// one that ran
    ran: HashSet<String>,
}

#[derive(Debug)]
struct RanInput {
    /// the `N` the input is named by, `<by-input-N>`
    number: u32,
    first_line: u32,
    line_count: u32,
}

/// Where an input sits in the module made of a session and that input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLines {
    /// the module line, 0-based, the input's first line is
    pub first: u32,
    /// how many lines the input is
    pub count: u32,
}

impl InputLines {
    /// whether `line` of the module is the input's. the input is the end of the module, so a
    /// line past its last is the input's too: the transpiler writes what follows a statement,
    /// such as a sealed class's list of its subclasses, against the line after it
    fn contains(self, line: u32) -> bool {
        line >= self.first
    }
}

/// What of the python a session and its new input came to runs for the input.
#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    /// the top-level statements of the python that run, by their index among them, in order
    pub statements: Vec<usize>,
    /// for each line of the python, 0-based, the line of the input it was written for, 0-based,
    /// or `None` for one written for no line of the input
    pub lines: Vec<Option<u32>>,
    /// the python of each statement that runs, which has run once the input is committed
    texts: Vec<String>,
}

/// Why the python written for an input cannot run.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// the python written for an earlier input, which has already run, came out differently
    /// with this one after it. `input` and `line` (1-based) say where
    ChangesAnEarlierInput { input: u32, line: u32 },
    /// one statement of the python was written for this input and an earlier one together
    JoinsAnEarlierInput { input: u32 },
    /// the python is not python: the transpiler wrote something no interpreter reads
    NotPython(String),
}

impl Session {
    /// The module made of the session's inputs: the session's source.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The source of the module made of the session and `input` after it, and where in it
    /// the input is. An input is a whole number of lines, so one that does not end in a line
    /// ending is given one.
    pub fn with_input(&self, input: &str) -> (String, InputLines) {
        let mut source = self.source.clone();
        let first = line_count(&source);
        source.push_str(input);
        if !input.ends_with('\n') {
            source.push('\n');
        }
        let count = line_count(&source) - first;
        (source, InputLines { first, count })
    }

    /// The input of the session that holds `line` of its source, 0-based, as the number it is
    /// named by and the line of it, 0-based. `None` for a line past the session's, which is
    /// the new input's.
    pub fn input_at(&self, line: u32) -> Option<(u32, u32)> {
        self.inputs
            .iter()
            .find(|input| line >= input.first_line && line - input.first_line < input.line_count)
            .map(|input| (input.number, line - input.first_line))
    }

    /// Which of `python`, what the module made of the session and the input at `input` was
    /// transpiled to, runs for the input. `line_map` is the transpile's: for each line of
    /// `python`, the module line it was written for.
    ///
    /// A top-level statement written for the input's lines runs. One written for an earlier
    /// input's has run already, and is skipped — unless it came out differently this time,
    /// which means the input changed what an earlier input meant after it ran, and refuses
    /// the input. One written for no line at all is the transpiler's own, a runtime helper
    /// or an import it needs, and runs the first time it appears.
    pub fn select(
        &self,
        python: &str,
        line_map: &[Option<u32>],
        input: InputLines,
    ) -> Result<Selection, Refusal> {
        let parsed = ruff_python_parser::parse_module(python)
            .map_err(|error| Refusal::NotPython(error.to_string()))?;
        let line_starts = line_starts(python);
        let line_of = |offset: TextSize| {
            let offset = usize::from(offset);
            u32::try_from(line_starts.partition_point(|&start| start <= offset) - 1)
                .unwrap_or(u32::MAX)
        };
        let mut statements = Vec::new();
        let mut texts = Vec::new();
        for (index, stmt) in parsed.suite().iter().enumerate() {
            let first = line_of(stmt.start());
            // the line of its last character, which a statement ending in a newline is not on
            let last =
                line_of(stmt.end().max(stmt.start() + TextSize::from(1)) - TextSize::from(1));
            let written_for: Vec<u32> = (first..=last)
                .filter_map(|line| line_map.get(line as usize).copied().flatten())
                .collect();
            let earlier = written_for.iter().find(|line| !input.contains(**line));
            let this = written_for.iter().any(|line| input.contains(*line));
            let text = &python[stmt.range()];
            match (this, earlier) {
                (true, Some(&line)) => {
                    return Err(Refusal::JoinsAnEarlierInput {
                        input: self.input_at(line).map_or(0, |(number, _)| number),
                    });
                }
                (false, Some(&line)) => {
                    if !self.ran.contains(text) {
                        let (input, line) = self.input_at(line).unwrap_or((0, line));
                        return Err(Refusal::ChangesAnEarlierInput {
                            input,
                            line: line + 1,
                        });
                    }
                }
                (true, None) => {
                    statements.push(index);
                    texts.push(text.to_owned());
                }
                (false, None) => {
                    if !self.ran.contains(text) {
                        statements.push(index);
                        texts.push(text.to_owned());
                    }
                }
            }
        }
        let lines = (0..line_starts.len())
            .map(|line| {
                line_map
                    .get(line)
                    .copied()
                    .flatten()
                    .filter(|line| input.contains(*line))
                    .map(|line| line - input.first)
            })
            .collect();
        Ok(Selection {
            statements,
            lines,
            texts,
        })
    }

    /// Make `input`, named `<by-input-{number}>`, part of the session, with the python
    /// `selection` picked for it as what has run.
    pub fn commit(&mut self, number: u32, input: &str, selection: Selection) {
        let (source, lines) = self.with_input(input);
        self.source = source;
        self.inputs.push(RanInput {
            number,
            first_line: lines.first,
            line_count: lines.count,
        });
        self.ran.extend(selection.texts);
    }
}

/// How many lines `text` is, counting a last line with no line ending.
fn line_count(text: &str) -> u32 {
    let endings = text.bytes().filter(|byte| *byte == b'\n').count();
    let unended = usize::from(!text.is_empty() && !text.ends_with('\n'));
    u32::try_from(endings + unended).unwrap_or(u32::MAX)
}

/// The offset each line of `text` starts at.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .filter(|&start| start < text.len() || start == 0)
        .collect()
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

#[cfg(test)]
mod selection_tests {
    use super::{InputLines, Refusal, Session};

    /// a session with `inputs` committed, each transpiled to itself line for line
    fn session_of(inputs: &[&str]) -> Session {
        let mut session = Session::default();
        for (number, input) in (1..).zip(inputs) {
            let (_, lines) = session.with_input(input);
            let (module, _) = session.with_input(input);
            let line_map: Vec<Option<u32>> = (0..lines.first + lines.count).map(Some).collect();
            let selection = session
                .select(&module, &line_map, lines)
                .expect("each input transpiled to itself");
            session.commit(number, input, selection);
        }
        session
    }

    #[test]
    fn an_input_runs_the_statements_written_for_its_lines() {
        let session = session_of(&["x = 1\n"]);
        let (module, lines) = session.with_input("y = x\nprint(y)");
        assert_eq!(module, "x = 1\ny = x\nprint(y)\n");
        assert_eq!(lines, InputLines { first: 1, count: 2 });
        let selection = session
            .select(&module, &[Some(0), Some(1), Some(2)], lines)
            .expect("runs");
        assert_eq!(selection.statements, [1, 2]);
        assert_eq!(selection.lines, [None, Some(0), Some(1)]);
    }

    /// a runtime helper or an import the transpiler wrote is written for no line, and runs the
    /// first time it appears
    #[test]
    fn what_the_transpiler_wrote_runs_once() {
        let mut session = Session::default();
        let (_, lines) = session.with_input("let MAX = 1\n");
        let python = "from typing import Final\nMAX: Final = 1\n";
        let selection = session
            .select(python, &[None, Some(0)], lines)
            .expect("runs");
        assert_eq!(selection.statements, [0, 1]);
        session.commit(1, "let MAX = 1\n", selection);

        let (_, lines) = session.with_input("let LIMIT = MAX\n");
        let python = "from typing import Final\nMAX: Final = 1\nLIMIT: Final = MAX\n";
        let selection = session
            .select(python, &[None, Some(0), Some(1)], lines)
            .expect("runs");
        assert_eq!(selection.statements, [2]);
        assert_eq!(selection.lines, [None, None, Some(0)]);
    }

    #[test]
    fn an_input_that_changes_an_earlier_one_is_refused() {
        let session = session_of(&["x = 1\n", "y = 2\n"]);
        let (_, lines) = session.with_input("z = 3\n");
        assert_eq!(
            session.select(
                "x = 1\ny = 2.0\nz = 3\n",
                &[Some(0), Some(1), Some(2)],
                lines
            ),
            Err(Refusal::ChangesAnEarlierInput { input: 2, line: 1 })
        );
    }

    #[test]
    fn a_statement_written_for_an_earlier_input_and_this_one_is_refused() {
        let session = session_of(&["x = 1\n"]);
        let (_, lines) = session.with_input("y = 2\n");
        assert_eq!(
            session.select("x = (1,\n    2)\n", &[Some(0), Some(1)], lines),
            Err(Refusal::JoinsAnEarlierInput { input: 1 })
        );
    }

    #[test]
    fn a_line_of_the_session_is_found_in_its_input() {
        let session = session_of(&["x = 1\n", "def f():\n    return x\n\n"]);
        assert_eq!(session.input_at(0), Some((1, 0)));
        assert_eq!(session.input_at(2), Some((2, 1)));
        assert_eq!(session.input_at(4), None);
    }
}
