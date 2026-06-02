use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// Strips explicit type arguments from generic function call sites.
///
///   `f[T](x)` → `f(x)`
///
/// only fires when the subscript target resolves to a locally-defined
/// function — avoids stripping constructor calls like `list[int](...)`.
pub(crate) struct GenericCallStrip<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> GenericCallStrip<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }
}

pub(crate) struct GenericCallStripPass<'src> {
    source: &'src str,
}

impl<'src> GenericCallStripPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for GenericCallStripPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = GenericCallStrip::new(self.source, types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
    }
}

impl<'ast> Visitor<'ast> for GenericCallStrip<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            if let Expr::Subscript(sub) = call.func.as_ref() {
                if let Expr::Name(name) = sub.value.as_ref() {
                    // a reified generic keeps its `[…]`: the `@generic` wrapper
                    // routes the specialization through `generic.__getitem__`,
                    // so stripping it would erase a runtime-significant step
                    if self.types.is_function(name) && !self.types.is_reified_function(name) {
                        let fn_src = self.src(sub.value.range()).to_owned();
                        self.edits
                            .push(Fix::safe_edit(Edit::range_replacement(fn_src, sub.range())));
                        // visit args but not the subscript func again
                        for arg in &call.arguments.args {
                            self.visit_expr(arg);
                        }
                        for kw in &call.arguments.keywords {
                            self.visit_expr(&kw.value);
                        }
                        return;
                    }
                }
            }
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn check_at(input: &str, expected: &str, version: PythonVersion) {
        let config = Config {
            min_version: version,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn generic_call_stripped() {
        check(
            indoc! {"
                def f[T](t1: T, t2: T) -> T: ...
                result = f[object](1, \"a\")
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                def f(t1: _T, t2: _T) -> _T: ...
                result = f(1, \"a\")
            "},
        );
    }

    #[test]
    fn generic_call_multiple_type_args() {
        check(
            indoc! {"
                def pair[A, B](a: A, b: B) -> tuple[A, B]: ...
                x = pair[int, str](1, \"a\")
            "},
            indoc! {"
                from typing import TypeVar
                _A = TypeVar(\"_A\")
                _B = TypeVar(\"_B\")
                def pair(a: _A, b: _B) -> tuple[_A, _B]: ...
                x = pair(1, \"a\")
            "},
        );
    }

    #[test]
    fn class_subscript_call_unchanged() {
        check(
            indoc! {"
                class Foo: ...
                x = Foo[int](1)
            "},
            indoc! {"
                class Foo: ...
                x = Foo[int](1)
            "},
        );
    }

    #[test]
    fn unresolved_call_unchanged() {
        check("x = y[int]()\n", "x = y[int]()\n");
    }

    #[test]
    fn generic_call_stripped_on_314() {
        // f[int]() is never valid Python; must strip even when min_version >= 3.12
        check_at(
            indoc! {"
                def f[T](): ...
                f[int]()
            "},
            indoc! {"
                def f[T](): ...
                f()
            "},
            PythonVersion::PY314,
        );
    }
}
