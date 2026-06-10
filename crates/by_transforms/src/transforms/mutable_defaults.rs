//! Text-edit pass: replaces non-scalar default arguments with a `_MISSING`
//! sentinel and injects a guard at the top of each function body.
//!
//!   def f(x=[]):        →   def f(x=_MISSING):
//!       ...                     if x is _MISSING:
//!                                   x = []
//!                               ...
//!
//! Only number, bool, None, string, and ellipsis literals (and unary +/-
//! on a number) are kept as-is; everything else is re-evaluated per call.
//!
//! The rewrite touches only the default expressions (each swapped for the
//! sentinel) and inserts the guard lines at the body start — the rest of the
//! function, body included, keeps its source bytes, so sibling lowerings
//! (`??`, `?.`, `int?` annotations, …) anywhere in the function still apply.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtFunctionDef, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::source_util::{line_indent, line_start};
use crate::type_info::TypeInfo;

fn is_immutable_scalar(expr: &Expr) -> bool {
    match expr {
        Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::UnaryOp(u)
            if matches!(u.op, UnaryOp::USub | UnaryOp::UAdd)
                && matches!(&*u.operand, Expr::NumberLiteral(_)) =>
        {
            true
        }
        _ => false,
    }
}

struct MutableDefaults<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
    used: bool,
}

impl MutableDefaults<'_> {
    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn process_function(&mut self, f: &StmtFunctionDef) {
        // (param name, default source) for each non-scalar default
        let mut guards: Vec<(String, String)> = Vec::new();
        let params = f.parameters.as_ref();
        for pw in params
            .posonlyargs
            .iter()
            .chain(params.args.iter())
            .chain(params.kwonlyargs.iter())
        {
            if let Some(d) = pw.default.as_deref()
                && !is_immutable_scalar(d)
            {
                self.edits.push((d.range(), "_MISSING".to_owned()));
                guards.push((
                    pw.parameter.name.id.to_string(),
                    self.src(d.range()).to_owned(),
                ));
            }
        }
        if guards.is_empty() {
            return;
        }
        self.used = true;

        // insert the guards at the start of the first non-docstring body
        // statement
        let docstring_count = if let Some(Stmt::Expr(e)) = f.body.first() {
            usize::from(matches!(e.value.as_ref(), Expr::StringLiteral(_)))
        } else {
            0
        };
        let mut text = String::new();
        if let Some(stmt) = f.body.get(docstring_count) {
            let insert_at = stmt.range().start();
            let prefix = &self.source
                [usize::from(line_start(self.source, insert_at))..usize::from(insert_at)];
            if prefix.trim().is_empty() {
                // the insertion lands after the statement's own indentation;
                // each guard re-establishes it for the following line
                let base = prefix.to_owned();
                for (name, default) in &guards {
                    text.push_str(&format!(
                        "if {name} is _MISSING:\n{base}    {name} = {default}\n{base}"
                    ));
                }
            } else {
                // single-line body (`def f(x=[]): ...`) — break it onto its own
                // indented line after the guards
                let base = format!("{}    ", line_indent(self.source, f.range().start()));
                for (name, default) in &guards {
                    text.push_str(&format!(
                        "\n{base}if {name} is _MISSING:\n{base}    {name} = {default}"
                    ));
                }
                text.push_str(&format!("\n{base}"));
            }
            self.edits.push((TextRange::empty(insert_at), text));
        } else {
            // docstring-only body: append the guards after it
            let doc_end = f.body[docstring_count - 1].range().end();
            let base = format!("{}    ", line_indent(self.source, f.range().start()));
            for (name, default) in &guards {
                text.push_str(&format!(
                    "\n{base}if {name} is _MISSING:\n{base}    {name} = {default}"
                ));
            }
            self.edits.push((TextRange::empty(doc_end), text));
        }
    }
}

impl<'ast> Visitor<'ast> for MutableDefaults<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            self.process_function(f);
        }
        walk_stmt(self, stmt);
    }
}

pub(crate) struct MutableDefaultsPass<'src> {
    source: &'src str,
}

impl<'src> MutableDefaultsPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for MutableDefaultsPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = MutableDefaults {
            source: self.source,
            edits: Vec::new(),
            used: false,
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if inner.used {
            ctx.required_imports.push("_MISSING = object()".to_owned());
        }
        ctx.text_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::transpile;
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &crate::Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn list_default() {
        check(
            indoc! {"
                def f(x=[]):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = []
                    pass
            "},
        );
    }

    #[test]
    fn dict_default() {
        check(
            indoc! {"
                def f(x={}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = {}
                    pass
            "},
        );
    }

    #[test]
    fn set_default() {
        check(
            indoc! {"
                def f(x={1, 2}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = {1, 2}
                    pass
            "},
        );
    }

    #[test]
    fn scalar_default_unchanged() {
        check(
            indoc! {"
                def f(x=0):
                    pass
            "},
            indoc! {"
                def f(x=0):
                    pass
            "},
        );
    }

    #[test]
    fn ellipsis_default_unchanged() {
        unchanged(indoc! {"
                def f(x=...):
                    pass
            "});
    }

    #[test]
    fn none_default_unchanged() {
        check(
            indoc! {"
                def f(x=None):
                    pass
            "},
            indoc! {"
                def f(x=None):
                    pass
            "},
        );
    }

    #[test]
    fn multiple_mutable_defaults() {
        check(
            indoc! {"
                def f(x=[], y={}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING, y=_MISSING):
                    if x is _MISSING:
                        x = []
                    if y is _MISSING:
                        y = {}
                    pass
            "},
        );
    }

    #[test]
    fn preserves_docstring() {
        check(
            indoc! {r#"
                def f(x=[]):
                    """doc"""
                    pass
            "#},
            indoc! {r#"
                _MISSING = object()
                def f(x=_MISSING):
                    """doc"""
                    if x is _MISSING:
                        x = []
                    pass
            "#},
        );
    }

    #[test]
    fn sentinel_defined_once_for_multiple_functions() {
        check(
            indoc! {"
                def f(x=[]):
                    pass
                def g(y={}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = []
                    pass
                def g(y=_MISSING):
                    if y is _MISSING:
                        y = {}
                    pass
            "},
        );
    }

    #[test]
    fn fstring_default() {
        check(
            indoc! {r#"
                data = "fdsa"
                def f(a=f"asdf{data}"):
                    print(a)
            "#},
            indoc! {r#"
                _MISSING = object()
                data = "fdsa"
                def f(a=_MISSING):
                    if a is _MISSING:
                        a = f"asdf{data}"
                    print(a)
            "#},
        );
    }

    #[test]
    fn tstring_default() {
        check(
            indoc! {r#"
                data = "fdsa"
                def f(a=t"asdf{data}"):
                    print(a)
            "#},
            indoc! {r#"
                _MISSING = object()
                data = "fdsa"
                def f(a=_MISSING):
                    if a is _MISSING:
                        a = t"asdf{data}"
                    print(a)
            "#},
        );
    }

    #[test]
    fn default_references_earlier_param() {
        // the signature keeps its source layout (only the default expression
        // is swapped for the sentinel)
        check(
            indoc! {"
                def f(a, b = a + 1):
                    print(a)


                f(1)
                f(2)
            "},
            indoc! {"
                _MISSING = object()
                def f(a, b = _MISSING):
                    if b is _MISSING:
                        b = a + 1
                    print(a)


                f(1)
                f(2)
            "},
        );
    }

    #[test]
    fn multiline_signature_inline_ellipsis_body() {
        // the signature keeps its source layout; the inline body breaks onto
        // its own line after the guard
        check(
            indoc! {"
                def f(
                    a: int = []
                ) -> int: ...
            "},
            "_MISSING = object()\ndef f(\n    a: int = _MISSING\n) -> int: \n    if a is _MISSING:\n        a = []\n    ...\n",
        );
    }

    #[test]
    fn inline_ellipsis_body() {
        check(
            "def f(x=[]): ...",
            "_MISSING = object()\ndef f(x=_MISSING): \n    if x is _MISSING:\n        x = []\n    ...",
        );
    }

    #[test]
    fn body_lowerings_survive() {
        // the body keeps its source bytes, so sibling lowerings inside it
        // (`int?`, `??`) still apply — previously the whole-def re-render
        // clobbered them
        check(
            indoc! {"
                def f(xs: list[int] = []) -> int:
                    a: int? = None
                    return a ?? len(xs)
            "},
            indoc! {"
                _MISSING = object()
                def f(xs: list[int] = _MISSING) -> int:
                    if xs is _MISSING:
                        xs = []
                    a: int | None = None
                    return a if a is not None else len(xs)
            "},
        );
    }
}
