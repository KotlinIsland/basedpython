use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::Db;
use crate::comma_list::{comma_list, tokens_within};

/// Returns a list of nested selection ranges, where each range contains the next one.
/// The first range in the list is the largest range containing the cursor position.
///
/// besides the nodes around the offset, the chain holds what a reader selects that the tree has
/// no node for:
///
/// - an element of a comma-separated list with its comma, `"a", ` in `["a", "b"]` (the last
///   element takes the comma before it), and then the whole list between its brackets
/// - a statement as the whole lines it is written on, indentation and newline included, when
///   nothing else shares them
/// - the statements of a suite, a body without its header, as whole lines
///
/// a range that does not contain the one before it is left out, so that the chain only ever
/// grows: the lines of a `def`'s body run on past the `def`'s own range, to the newline after its
/// last line, and the next range up is the `def` as whole lines
pub fn selection_range(db: &dyn Db, file: PythonFile<'_>, offset: TextSize) -> Vec<TextRange> {
    let parsed = parsed_module(db, file).load(db);
    let source = source_text(db, file.file(db));
    let source = source.as_str();
    let tokens = parsed.tokens();

    let covering = covering_node(parsed.syntax().into(), TextRange::empty(offset));

    let mut candidates = Vec::new();
    for node in covering.ancestors() {
        if should_include_in_selection(node) {
            candidates.push(node.range());
            if let Some(literal_range) = literal_content_range(node, offset) {
                candidates.push(literal_range);
            }
        }
        if let Some(list) = comma_list(node, tokens) {
            if let Some(element) = list
                .elements
                .iter()
                .position(|element| element.contains_inclusive(offset))
                .and_then(|index| element_with_comma(&list.elements, index, tokens, source))
            {
                candidates.push(element);
            }
            if list.brackets.is_some()
                && let (Some(first), Some(last)) = (list.elements.first(), list.elements.last())
            {
                candidates.push(first.cover(*last));
            }
        }
        if node.is_statement() {
            candidates.extend(whole_lines(node.range(), source));
        }
        for suite in suites(node) {
            if let (Some(first), Some(last)) = (suite.first(), suite.last())
                && first.range().cover(last.range()).contains_inclusive(offset)
            {
                candidates.extend(whole_lines(first.range().cover(last.range()), source));
            }
        }
    }

    // smallest first, keeping each range that contains the one kept before it
    candidates.sort_by_key(|range| (range.len(), range.start()));
    let mut chain: Vec<TextRange> = Vec::with_capacity(candidates.len());
    for range in candidates {
        if chain
            .last()
            .is_none_or(|inner| range != *inner && range.contains_range(*inner))
        {
            chain.push(range);
        }
    }
    chain.reverse();
    chain
}

/// Determines if a node should be included in the selection range hierarchy.
/// This filters out intermediate nodes that don't provide meaningful selections.
fn should_include_in_selection(node: AnyNodeRef) -> bool {
    // We will likely need to tune this based on user feedback. Some users may
    // prefer finer-grained selections while others may prefer coarser-grained.
    match node {
        // Exclude nodes that don't represent meaningful semantic units for selection
        AnyNodeRef::StmtExpr(_) => false,

        _ => true,
    }
}

/// Determines the content range of a valid string or byte literal if it's non-empty.
fn literal_content_range(node: AnyNodeRef, offset: TextSize) -> Option<TextRange> {
    let range = match node {
        AnyNodeRef::StringLiteral(s) if !s.flags.is_invalid() => Some(s.content_range()),
        AnyNodeRef::BytesLiteral(b) if !b.flags.is_invalid() => Some(b.content_range()),
        _ => None,
    };
    range.filter(|&range| !range.is_empty() && range.contains_inclusive(offset))
}

/// the element at `index` of a list with the comma that separates it from the next one, and
/// the spaces after that comma, so that deleting the selection leaves the list well formed. the
/// last element, with no comma after it, takes the comma before it instead. `None` for an
/// element no comma separates from another, the only one of its list
fn element_with_comma(
    elements: &[TextRange],
    index: usize,
    tokens: &Tokens,
    source: &str,
) -> Option<TextRange> {
    let element = *elements.get(index)?;
    let next_bound = elements
        .get(index + 1)
        .map_or(TextSize::of(source), Ranged::start);
    let after = tokens_within(tokens, TextRange::new(element.end(), next_bound))
        .iter()
        .find(|token| !token.kind().is_trivia());
    if let Some(comma) = after.filter(|token| token.kind() == TokenKind::Comma) {
        let spaces = source[comma.end().to_usize()..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let end = comma.end() + TextSize::try_from(spaces).ok()?;
        return Some(TextRange::new(element.start(), end));
    }
    let previous = elements.get(index.checked_sub(1)?)?;
    let comma = tokens_within(tokens, TextRange::new(previous.end(), element.start()))
        .iter()
        .find(|token| token.kind() == TokenKind::Comma)?;
    Some(TextRange::new(comma.start(), element.end()))
}

/// `range` as the whole lines it is written on, from the start of its first line to past the
/// newline that ends its last, when nothing but indentation comes before it on its first line
/// and nothing but a comment after it on its last
fn whole_lines(range: TextRange, source: &str) -> Option<TextRange> {
    let line_start = source.line_start(range.start());
    let before = &source[TextRange::new(line_start, range.start())];
    if !before.chars().all(|char| matches!(char, ' ' | '\t')) {
        return None;
    }
    let after = source[TextRange::new(range.end(), source.line_end(range.end()))].trim_start();
    if !(after.is_empty() || after.starts_with('#')) {
        return None;
    }
    Some(TextRange::new(
        line_start,
        source.full_line_end(range.end()),
    ))
}

/// the suites of a compound statement, or of the clause of one: a body without its header
fn suites(node: AnyNodeRef<'_>) -> Vec<&[ast::Stmt]> {
    match node {
        AnyNodeRef::StmtFunctionDef(function) => vec![&function.body[..]],
        AnyNodeRef::StmtClassDef(class) => vec![&class.body[..]],
        AnyNodeRef::StmtFor(for_stmt) => vec![&for_stmt.body[..], &for_stmt.orelse[..]],
        AnyNodeRef::StmtWhile(while_stmt) => vec![&while_stmt.body[..], &while_stmt.orelse[..]],
        AnyNodeRef::StmtIf(if_stmt) => vec![&if_stmt.body[..]],
        AnyNodeRef::ElifElseClause(clause) => vec![&clause.body[..]],
        AnyNodeRef::StmtWith(with) => vec![&with.body[..]],
        AnyNodeRef::StmtTry(try_stmt) => vec![
            &try_stmt.body[..],
            &try_stmt.orelse[..],
            &try_stmt.finalbody[..],
        ],
        AnyNodeRef::ExceptHandlerExceptHandler(handler) => vec![&handler.body[..]],
        AnyNodeRef::MatchCase(case) => vec![&case.body[..]],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::CursorTest;
    use insta::assert_snapshot;
    use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, LintName, Severity, Span};
    use ruff_db::files::FileRange;
    use ruff_text_size::Ranged;

    /// Test selection range on a simple expression
    #[test]
    fn selection_range_simple_expression() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
x = 1 + <CURSOR>2
",
            )
            .build();

        assert_snapshot!(test.selection_range(), @"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | x = 1 + 2
          | |__________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | x = 1 + 2
          | ^^^^^^^^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | x = 1 + 2
          | ^^^^^^^^^

        info[selection-range]: Selection Range 3
         --> main.py:2:5
          |
        2 | x = 1 + 2
          |     ^^^^^

        info[selection-range]: Selection Range 4
         --> main.py:2:9
          |
        2 | x = 1 + 2
          |         ^
        ");
    }

    /// Test selection range on a function call
    #[test]
    fn selection_range_function_call() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
print(\"he<CURSOR>llo\")
",
            )
            .build();

        assert_snapshot!(test.selection_range(), @r#"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | print("hello")
          | |_______________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | print("hello")
          | ^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | print("hello")
          | ^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 3
         --> main.py:2:6
          |
        2 | print("hello")
          |      ^^^^^^^^^

        info[selection-range]: Selection Range 4
         --> main.py:2:7
          |
        2 | print("hello")
          |       ^^^^^^^

        info[selection-range]: Selection Range 5
         --> main.py:2:8
          |
        2 | print("hello")
          |        ^^^^^
        "#);
    }

    /// Test selection range when the cursor is in a string literal prefix
    #[test]
    fn selection_range_string_literal_prefix() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
r<CURSOR>"hello"
"#,
            )
            .build();

        assert_snapshot!(test.selection_range(), @r#"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | r"hello"
          | |_________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | r"hello"
          | ^^^^^^^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | r"hello"
          | ^^^^^^^^
        "#);
    }

    /// Test selection range on a string literal created during parser recovery
    #[test]
    fn selection_range_recovered_invalid_string_literal() {
        let test = CursorTest::builder()
            .source("main.py", "f\"foo\" b\"b<CURSOR>ar\"")
            .build();

        assert_snapshot!(test.selection_range(), @r#"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | f"foo" b"bar"
          | ^^^^^^^^^^^^^

        info[selection-range]: Selection Range 1
         --> main.py:1:8
          |
        1 | f"foo" b"bar"
          |        ^^^^^^
        "#);
    }

    /// Test selection range on a function definition
    #[test]
    fn selection_range_function_definition() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
def my_<CURSOR>function():
    return 42
",
            )
            .build();

        assert_snapshot!(test.selection_range(), @"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | def my_function():
        3 | |     return 42
          | |______________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | / def my_function():
        3 | |     return 42
          | |______________^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | / def my_function():
        3 | |     return 42
          | |_____________^

        info[selection-range]: Selection Range 3
         --> main.py:2:5
          |
        2 | def my_function():
          |     ^^^^^^^^^^^
        ");
    }

    /// Test selection range on a class definition
    #[test]
    fn selection_range_class_definition() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class My<CURSOR>Class:
    def __init__(self):
        self.value = 1
",
            )
            .build();

        assert_snapshot!(test.selection_range(), @"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | class MyClass:
        3 | |     def __init__(self):
        4 | |         self.value = 1
          | |_______________________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | / class MyClass:
        3 | |     def __init__(self):
        4 | |         self.value = 1
          | |_______________________^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | / class MyClass:
        3 | |     def __init__(self):
        4 | |         self.value = 1
          | |______________________^

        info[selection-range]: Selection Range 3
         --> main.py:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^
        ");
    }

    /// Test selection range on a deeply nested expression with comprehension, lambda, and subscript
    #[test]
    fn selection_range_deeply_nested_expression() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
result = [(lambda x: x[key.<CURSOR>attr])(item) for item in data if item is not None]
",
            )
            .build();

        assert_snapshot!(test.selection_range(), @"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          | |______________________________________________________________________________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 3
         --> main.py:2:10
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          |          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 4
         --> main.py:2:11
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          |           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 5
         --> main.py:2:12
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          |            ^^^^^^^^^^^^^^^^^^^^^

        info[selection-range]: Selection Range 6
         --> main.py:2:22
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          |                      ^^^^^^^^^^^

        info[selection-range]: Selection Range 7
         --> main.py:2:24
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          |                        ^^^^^^^^

        info[selection-range]: Selection Range 8
         --> main.py:2:28
          |
        2 | result = [(lambda x: x[key.attr])(item) for item in data if item is not None]
          |                            ^^^^
        ");
    }

    /// Test selection range on an empty string literal
    #[test]
    fn selection_range_empty_string_literal() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
"<CURSOR>"
"#,
            )
            .build();

        assert_snapshot!(test.selection_range(), @r#"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | ""
          | |___^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | ""
          | ^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | ""
          | ^^
        "#);
    }

    /// Test selection range on a bytes literal
    #[test]
    fn selection_range_bytes_literal() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
b"he<CURSOR>llo"
"#,
            )
            .build();

        assert_snapshot!(test.selection_range(), @r#"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | b"hello"
          | |_________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | b"hello"
          | ^^^^^^^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | b"hello"
          | ^^^^^^^^

        info[selection-range]: Selection Range 3
         --> main.py:2:3
          |
        2 | b"hello"
          |   ^^^^^
        "#);
    }

    /// Test selection range on an invalid bytes literal
    #[test]
    fn selection_range_invalid_bytes_literal() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
b"123a𝐁<CURSOR>c"
"#,
            )
            .build();

        assert_snapshot!(test.selection_range(), @r#"
        info[selection-range]: Selection Range 0
         --> main.py:1:1
          |
        1 | /
        2 | | b"123a𝐁c"
          | |__________^

        info[selection-range]: Selection Range 1
         --> main.py:2:1
          |
        2 | b"123a𝐁c"
          | ^^^^^^^^^

        info[selection-range]: Selection Range 2
         --> main.py:2:1
          |
        2 | b"123a𝐁c"
          | ^^^^^^^^^
        "#);
    }

    /// an element of a list is selected with its comma, and the elements together without the
    /// brackets, before the whole list
    #[test]
    fn selection_range_list_element_with_comma() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
specs = ["squ<CURSOR>are", "circle"]
"#,
            )
            .build();

        assert_snapshot!(test.selection_texts(), @r#"
        |square|
        |"square"|
        |"square", |
        |"square", "circle"|
        |["square", "circle"]|
        |specs = ["square", "circle"]|
        |specs = ["square", "circle"]⏎|
        |⏎specs = ["square", "circle"]⏎|
        "#);
    }

    /// the last element has no comma after it, and takes the one before it
    #[test]
    fn selection_range_last_element_takes_the_comma_before_it() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
print(width, hei<CURSOR>ght)
"#,
            )
            .build();

        assert_snapshot!(test.selection_texts(), @"
        |height|
        |, height|
        |width, height|
        |(width, height)|
        |print(width, height)|
        |print(width, height)⏎|
        |⏎print(width, height)⏎|
        ");
    }

    /// a statement is selected as its whole line, and a body as its lines without the header,
    /// before the decorated method as whole lines
    #[test]
    fn selection_range_statement_line_and_suite() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
class Shape:
    @property
    def area(self) -> int:
        size = 2
        return si<CURSOR>ze * size  # squared
"#,
            )
            .build();

        assert_snapshot!(test.selection_texts(), @"
        |size|
        |size * size|
        |return size * size|
        |        return size * size  # squared⏎|
        |        size = 2⏎        return size * size  # squared⏎|
        |    @property⏎    def area(self) -> int:⏎        size = 2⏎        return size * size  # squared⏎|
        |class Shape:⏎    @property⏎    def area(self) -> int:⏎        size = 2⏎        return size * size  # squared⏎|
        |⏎class Shape:⏎    @property⏎    def area(self) -> int:⏎        size = 2⏎        return size * size  # squared⏎|
        ");
    }

    /// a statement that shares its line with another is not selected as the line
    #[test]
    fn selection_range_statement_sharing_its_line() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
if ready: sta<CURSOR>rt()
"#,
            )
            .build();

        assert_snapshot!(test.selection_texts(), @"
        |start|
        |start()|
        |if ready: start()|
        |if ready: start()⏎|
        |⏎if ready: start()⏎|
        ");
    }

    impl CursorTest {
        /// each range of the chain as the text it selects, innermost first, between `|`s and with
        /// each newline written `⏎`
        fn selection_texts(&self) -> String {
            let python_file = self.program_file(self.cursor.file).python_file(&self.db);
            let source = ruff_db::source::source_text(&self.db, self.cursor.file);
            selection_range(&self.db, python_file, self.cursor.offset)
                .iter()
                .rev()
                .map(|&range| format!("|{}|", source.as_str()[range].replace('\n', "⏎")))
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn selection_range(&self) -> String {
            let ranges = selection_range(
                &self.db,
                self.program_file(self.cursor.file).python_file(&self.db),
                self.cursor.offset,
            );

            if ranges.is_empty() {
                return "No selection range found".to_string();
            }

            // Create one diagnostic per range for clearer visualization
            let diagnostics: Vec<SelectionRangeDiagnostic> = ranges
                .iter()
                .enumerate()
                .map(|(index, &range)| {
                    SelectionRangeDiagnostic::new(FileRange::new(self.cursor.file, range), index)
                })
                .collect();

            self.render_diagnostics(diagnostics)
        }
    }

    struct SelectionRangeDiagnostic {
        range: FileRange,
        index: usize,
    }

    impl SelectionRangeDiagnostic {
        fn new(range: FileRange, index: usize) -> Self {
            Self { range, index }
        }
    }

    impl crate::tests::IntoDiagnostic for SelectionRangeDiagnostic {
        fn into_diagnostic(self) -> Diagnostic {
            let mut diagnostic = Diagnostic::new(
                DiagnosticId::Lint(LintName::of("selection-range")),
                Severity::Info,
                format!("Selection Range {}", self.index),
            );

            diagnostic.annotate(Annotation::primary(
                Span::from(self.range.file()).with_range(self.range.range()),
            ));

            diagnostic
        }
    }
}
