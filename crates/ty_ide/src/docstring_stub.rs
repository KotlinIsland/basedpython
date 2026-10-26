//! the docstring an editor writes into an empty one: a line for the summary, then a section
//! for the parameters and one for what the function returns, in the docstring format the
//! project writes
//!
//! ```python
//! def area(width: float, height: float) -> float:
//!     """
//!     |
//!
//!     :param width:
//!     :param height:
//!     :return:
//!     """
//! ```

use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt, StringFlags};
use ruff_python_trivia::indentation_at_offset;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_core::ProgramFile;
use ty_python_semantic::SemanticModel;

use crate::Db;
use crate::method_kind::{MethodKind, resolved_method_kind};

/// a docstring convention a stub can be written in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocstringFormat {
    /// reStructuredText fields, `:param width:` and `:return:`, as Sphinx reads them
    Rest,
    /// Google style, an `Args:` and a `Returns:` section
    Google,
    /// NumPy style, `Parameters` and `Returns` sections underlined with dashes
    Numpy,
    /// the summary line alone
    Plain,
}

/// the rewrite of an empty docstring into a stub
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocstringStub {
    /// the docstring literal, quotes included, which `text` replaces
    pub range: TextRange,
    pub text: String,
    /// the line of the stub the summary goes on, counted from the line the literal starts on,
    /// and the column it starts at — the indentation, which is ASCII whitespace and so the same
    /// in every position encoding
    pub caret_line: u32,
    pub caret_column: u32,
}

/// the stub for the empty docstring of the `def` whose docstring `offset` is in, in `format`,
/// or in the format the file's other docstrings are written in when `format` is `None`, and
/// reStructuredText when none of them says
///
/// `None` unless `offset` is inside a triple-quoted string literal holding nothing but
/// whitespace that is the first statement of a function's body and starts its own line
pub fn docstring_stub(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    format: Option<DocstringFormat>,
) -> Option<DocstringStub> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let source = source_text(db, file.file(db));
    let source = source.as_str();

    let mut finder = DocstringFinder {
        offset,
        class_body: Vec::new(),
        found: None,
        formats: Vec::new(),
    };
    finder.visit_body(parsed.suite());
    let found = finder.found?;
    let literal = found.literal;

    let [part] = literal.value.as_slice() else {
        return None;
    };
    if !part.flags.is_triple_quoted() || !literal.value.to_str().trim().is_empty() {
        return None;
    }
    let indentation = indentation_at_offset(literal.start(), source)?;

    let method = if found.in_class {
        Some(resolved_method_kind(
            &SemanticModel::new(db, file),
            found.function,
        )?)
    } else {
        None
    };
    let skips_receiver = match method {
        Some(MethodKind::Instance | MethodKind::Class) => true,
        // `__new__` is passed its class explicitly, as its first argument
        Some(MethodKind::Static) => found.function.name.as_str() == "__new__",
        None => false,
    };
    let function = found.function;
    let parameters = &function.parameters;
    // each parameter as the stub shows it, `*args` with its stars, and by its name alone
    let named = |parameter: &ast::Parameter, stars: &str| {
        (
            format!("{stars}{}", parameter.name),
            parameter.name.to_string(),
        )
    };
    let mut written: Vec<(String, String)> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .skip(usize::from(skips_receiver))
        .map(|parameter| named(&parameter.parameter, ""))
        .collect();
    written.extend(
        parameters
            .vararg
            .as_deref()
            .map(|vararg| named(vararg, "*")),
    );
    written.extend(
        parameters
            .kwonlyargs
            .iter()
            .map(|parameter| named(&parameter.parameter, "")),
    );
    written.extend(parameters.kwarg.as_deref().map(|kwarg| named(kwarg, "**")));
    written.retain(|(_, name)| !name.is_empty());

    let result = function_result(function);
    let format = format
        .or_else(|| finder.formats.first().copied())
        .unwrap_or(DocstringFormat::Rest);

    let quotes = &source[TextRange::at(
        literal.start() + part.flags.prefix().text_len(),
        part.flags.quote_len(),
    )];
    let prefix = part.flags.prefix().as_str();
    let line_ending =
        ruff_source_file::find_newline(source).map_or("\n", |(_, ending)| ending.as_str());

    let mut lines: Vec<String> = vec![String::new()];
    let sections = format != DocstringFormat::Plain && (!written.is_empty() || result.is_some());
    if sections {
        lines.push(String::new());
        match format {
            DocstringFormat::Rest => {
                for (_, name) in &written {
                    lines.push(format!(":param {name}:"));
                }
                match result {
                    Some(FunctionResult::Returns) => lines.push(":return:".to_string()),
                    Some(FunctionResult::Yields) => lines.push(":yield:".to_string()),
                    None => {}
                }
            }
            DocstringFormat::Google => {
                if !written.is_empty() {
                    lines.push("Args:".to_string());
                    for (shown, _) in &written {
                        lines.push(format!("    {shown}:"));
                    }
                }
                if let Some(result) = result {
                    if !written.is_empty() {
                        lines.push(String::new());
                    }
                    lines.push(match result {
                        FunctionResult::Returns => "Returns:".to_string(),
                        FunctionResult::Yields => "Yields:".to_string(),
                    });
                }
            }
            DocstringFormat::Numpy => {
                if !written.is_empty() {
                    lines.push("Parameters".to_string());
                    lines.push("----------".to_string());
                    for (shown, _) in &written {
                        lines.push(shown.clone());
                    }
                }
                if let Some(result) = result {
                    if !written.is_empty() {
                        lines.push(String::new());
                    }
                    let heading = match result {
                        FunctionResult::Returns => "Returns",
                        FunctionResult::Yields => "Yields",
                    };
                    lines.push(heading.to_string());
                    lines.push("-".repeat(heading.len()));
                }
            }
            DocstringFormat::Plain => {}
        }
    }

    let mut text = format!("{prefix}{quotes}{line_ending}");
    for (index, line) in lines.iter().enumerate() {
        // the summary line is indented although empty, since the caret is put at its end
        if index == 0 || !line.is_empty() {
            text.push_str(indentation);
            text.push_str(line);
        }
        text.push_str(line_ending);
    }
    text.push_str(indentation);
    text.push_str(quotes);

    Some(DocstringStub {
        range: literal.range(),
        text,
        caret_line: 1,
        caret_column: u32::try_from(indentation.len()).ok()?,
    })
}

/// what a function hands its caller that a docstring documents
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionResult {
    Returns,
    Yields,
}

/// whether `function` returns a value, yields them, or neither: a generator yields; otherwise a
/// return annotation other than `None`, or without one a `return` of something other than
/// `None`, is a value returned
fn function_result(function: &ast::StmtFunctionDef) -> Option<FunctionResult> {
    #[derive(Default)]
    struct Body {
        yields: bool,
        returns_value: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Body {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            match node {
                // what a nested function or class does is its own
                AnyNodeRef::StmtFunctionDef(_)
                | AnyNodeRef::StmtClassDef(_)
                | AnyNodeRef::ExprLambda(_) => TraversalSignal::Skip,
                AnyNodeRef::ExprYield(_) | AnyNodeRef::ExprYieldFrom(_) => {
                    self.yields = true;
                    TraversalSignal::Traverse
                }
                AnyNodeRef::StmtReturn(ret) => {
                    if ret
                        .value
                        .as_deref()
                        .is_some_and(|value| !value.is_none_literal_expr())
                    {
                        self.returns_value = true;
                    }
                    TraversalSignal::Traverse
                }
                _ => TraversalSignal::Traverse,
            }
        }
    }
    let mut body = Body::default();
    body.visit_body(&function.body);
    if body.yields {
        return Some(FunctionResult::Yields);
    }
    match function.returns.as_deref() {
        Some(Expr::NoneLiteral(_)) => None,
        Some(Expr::Name(name)) if matches!(name.id.as_str(), "NoReturn" | "Never") => None,
        Some(_) => Some(FunctionResult::Returns),
        None => body.returns_value.then_some(FunctionResult::Returns),
    }
}

struct Found<'a> {
    function: &'a ast::StmtFunctionDef,
    literal: &'a ast::ExprStringLiteral,
    /// whether a class body defines the function, as a method
    in_class: bool,
}

/// finds the docstring around the offset, and the formats the file's docstrings are written
/// in, in source order
struct DocstringFinder<'a> {
    offset: TextSize,
    /// for each suite being walked, whether it is a class body
    class_body: Vec<bool>,
    found: Option<Found<'a>>,
    formats: Vec<DocstringFormat>,
}

impl<'a> DocstringFinder<'a> {
    fn docstring(body: &'a [Stmt]) -> Option<&'a ast::ExprStringLiteral> {
        match body.first()? {
            Stmt::Expr(stmt) => stmt.value.as_string_literal_expr(),
            _ => None,
        }
    }

    fn note_format(&mut self, body: &'a [Stmt]) {
        if let Some(docstring) = Self::docstring(body)
            && let Some(format) = written_format(docstring.value.to_str())
        {
            self.formats.push(format);
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for DocstringFinder<'a> {
    fn visit_body(&mut self, body: &'a [Stmt]) {
        self.note_format(body);
        source_order::walk_body(self, body);
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                if self.found.is_none()
                    && let Some(literal) = Self::docstring(&function.body)
                    && literal.start() < self.offset
                    && self.offset < literal.end()
                {
                    let in_class = self.class_body.last().copied().unwrap_or(false);
                    self.found = Some(Found {
                        function,
                        literal,
                        in_class,
                    });
                }
                self.class_body.push(false);
                source_order::walk_stmt(self, stmt);
                self.class_body.pop();
            }
            Stmt::ClassDef(_) => {
                self.class_body.push(true);
                source_order::walk_stmt(self, stmt);
                self.class_body.pop();
            }
            _ => {
                // a nested suite of an `if` in a class body is still the class's
                source_order::walk_stmt(self, stmt);
            }
        }
    }
}

/// the convention a docstring is written in, when it says
fn written_format(docstring: &str) -> Option<DocstringFormat> {
    let lines: Vec<&str> = docstring.lines().map(str::trim).collect();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with(":param ")
            || line.starts_with(":return")
            || line.starts_with(":rtype:")
            || line.starts_with(":raises ")
        {
            return Some(DocstringFormat::Rest);
        }
        if matches!(
            *line,
            "Args:" | "Arguments:" | "Returns:" | "Yields:" | "Raises:"
        ) {
            return Some(DocstringFormat::Google);
        }
        if matches!(*line, "Parameters" | "Returns" | "Yields" | "Raises")
            && lines
                .get(index + 1)
                .is_some_and(|next| next.len() >= 3 && next.chars().all(|c| c == '-'))
        {
            return Some(DocstringFormat::Numpy);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{DocstringFormat, docstring_stub};
    use crate::tests::CursorTest;
    use insta::assert_snapshot;

    fn stub(source: &str, format: Option<DocstringFormat>) -> String {
        let test = CursorTest::builder().source("main.py", source).build();
        let Some(stub) = docstring_stub(
            &test.db,
            test.program_file(test.cursor.file),
            test.cursor.offset,
            format,
        ) else {
            return "no stub".to_string();
        };
        let text = ruff_db::source::source_text(&test.db, test.cursor.file);
        let mut rewritten = text.as_str().to_string();
        rewritten.replace_range(
            stub.range.start().to_usize()..stub.range.end().to_usize(),
            &stub.text,
        );
        format!(
            "caret at line {} of the stub, column {}\n{rewritten}",
            stub.caret_line, stub.caret_column
        )
    }

    #[test]
    fn rest_by_default() {
        assert_snapshot!(stub(
            r#"
def area(width: float, height: float) -> float:
    """
    <CURSOR>
    """
    return width * height
"#,
            None,
        ), @r#"
        caret at line 1 of the stub, column 4

        def area(width: float, height: float) -> float:
            """
            

            :param width:
            :param height:
            :return:
            """
            return width * height
        "#);
    }

    #[test]
    fn google_skips_the_receiver_and_stars_the_variadics() {
        assert_snapshot!(stub(
            r#"
class Shape:
    def scale(self, factor: int, *args, key: str, **kwargs):
        """<CURSOR>"""
        return self
"#,
            Some(DocstringFormat::Google),
        ), @r#"
        caret at line 1 of the stub, column 8

        class Shape:
            def scale(self, factor: int, *args, key: str, **kwargs):
                """
                

                Args:
                    factor:
                    *args:
                    key:
                    **kwargs:

                Returns:
                """
                return self
        "#);
    }

    #[test]
    fn numpy_for_a_generator() {
        assert_snapshot!(stub(
            r#"
def count(limit):
    """<CURSOR>"""
    yield limit
"#,
            Some(DocstringFormat::Numpy),
        ), @r#"
        caret at line 1 of the stub, column 4

        def count(limit):
            """
            

            Parameters
            ----------
            limit

            Yields
            ------
            """
            yield limit
        "#);
    }

    /// another docstring in the file says which format the project writes
    #[test]
    fn the_file_s_other_docstrings_choose_the_format() {
        assert_snapshot!(stub(
            r#"
def first(a):
    """Do it.

    Args:
        a: the thing
    """

class Builder:
    @classmethod
    def build(cls, name: str) -> None:
        '''<CURSOR>'''
"#,
            None,
        ), @r#"
        caret at line 1 of the stub, column 8

        def first(a):
            """Do it.

            Args:
                a: the thing
            """

        class Builder:
            @classmethod
            def build(cls, name: str) -> None:
                '''
                

                Args:
                    name:
                '''
        "#);
    }

    /// a static method is told by what its decorator resolves to, and its first parameter is
    /// the caller's, under whatever name `staticmethod` is bound to
    #[test]
    fn a_static_method_under_another_name_documents_its_first_parameter() {
        assert_snapshot!(stub(
            r#"
static = staticmethod

class Builder:
    @static
    def build(name: str, size: int) -> None:
        """<CURSOR>"""
"#,
            None,
        ), @r#"
        caret at line 1 of the stub, column 8

        static = staticmethod

        class Builder:
            @static
            def build(name: str, size: int) -> None:
                """
                

                :param name:
                :param size:
                """
        "#);
    }

    #[test]
    fn a_written_docstring_gets_no_stub() {
        assert_snapshot!(stub(
            r#"
def f(a):
    """Already <CURSOR>said."""
"#,
            None,
        ), @"no stub");
    }

    #[test]
    fn a_string_that_is_not_a_function_s_docstring_gets_no_stub() {
        assert_snapshot!(stub(
            r#"
def f(a):
    print(a)
    """<CURSOR>"""
"#,
            None,
        ), @"no stub");
    }
}
