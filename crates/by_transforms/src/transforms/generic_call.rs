use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, ExprContext, PythonVersion, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// Strips explicit type arguments from a subscript of a generic function that erases them,
/// wherever the subscript stands, since python has nothing to subscript.
///
///   `f[T](x)` → `f(x)`
///   `y = f[T]` → `y = f`
///
/// only fires when the subscript target resolves to a generic function — avoids stripping
/// constructor calls like `list[int](...)`, and a reified function keeps its `[…]`: the
/// `@generic` wrapper routes the specialization through `generic.__getitem__`.
///
/// each edit deletes what it covers, so a lowering of a type argument inside one has nothing
/// to reach the output with, and no type argument is evaluated
struct GenericCallStrip<'src> {
    types: &'src dyn TypeInfo,
    min_version: PythonVersion,
    edits: Vec<(TextRange, String)>,
}

pub(crate) struct GenericCallStripPass {
    min_version: PythonVersion,
}

impl GenericCallStripPass {
    pub(crate) fn new(min_version: PythonVersion) -> Self {
        Self { min_version }
    }
}

impl TypeAwarePass for GenericCallStripPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = GenericCallStrip {
            types,
            min_version: self.min_version,
            edits: Vec::new(),
        };
        inner.visit_body(stmts);
        ctx.text_edits.extend(inner.edits);
    }
}

impl<'ast> Visitor<'ast> for GenericCallStrip<'_> {
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // a `type def` applied is a type, which the lowering of a type writes
        if let Expr::Subscript(subscript) = expr
            && subscript.ctx == ExprContext::Load
            && !self.types.is_type_fn_application(expr)
            && self
                .types
                .subscript_erases_type_arguments(&subscript.value, self.min_version)
        {
            // these take any parentheses around the value with them, on both of its sides
            let value = subscript.value.range();
            let opening = TextRange::new(subscript.start(), value.start());
            if !opening.is_empty() {
                self.edits.push((opening, String::new()));
            }
            self.edits
                .push((TextRange::new(value.end(), subscript.end()), String::new()));
            self.visit_expr(&subscript.value);
            return;
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
    fn method_call_stripped() {
        check(
            indoc! {"
                class K:
                    def m[T](self, x: T) -> T:
                        return x
                    class def c[T](cls, x: T) -> T:
                        return x
                a = K().m[int](1)
                b = K.c[int](1)
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                class K:
                    def m(self, x: _T) -> _T:
                        return x
                    @classmethod
                    def c(cls, x: _T) -> _T:
                        return x
                a = K().m(1)
                b = K.c(1)
            "},
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

    #[test]
    fn a_subscript_is_stripped_wherever_it_stands() {
        check_at(
            indoc! {"
                def f[T](x: T) -> T:
                    return x
                y = f[int]
                print(f[1])
                z = map(f[int], [1])
                g = f
                print(g[1])
                print((f)[int])
            "},
            indoc! {"
                def f[T](x: T) -> T:
                    return x
                y = f
                print(f)
                z = map(f, [1])
                g = f
                print(g)
                print(f)
            "},
            PythonVersion::PY314,
        );
    }

    #[test]
    fn what_a_stripped_subscript_holds_is_not_lowered() {
        let source = indoc! {"
            from collections.abc import Sequence
            def f[T](t: T) -> T:
                return t
            f[map[int]](map(int, []))
            f[Sequence[int]]([1])
            f[list[1]](1)
            f[int?](1)
            f[() -> int](1)
        "};
        let config = Config {
            min_version: PythonVersion::PY314,
            ..Config::default()
        };
        let out = transpile(source, &config).unwrap();
        assert!(
            out.contains("f(map(int, []))\nf([1])\nf(1)\nf(1)\nf(1)\n"),
            "{out}"
        );
        for lowered in ["_by_subscript(map", "_by_variance(1)(Sequence)", "Literal"] {
            assert!(
                !out[out.rfind("def f").unwrap()..].contains(lowered),
                "{out}"
            );
        }
    }

    #[test]
    fn a_function_that_is_not_generic_is_left_to_fail_as_written() {
        check(
            "def h(x: int): ...\nh[int](1)\n",
            "def h(x: int): ...\nh[int](1)\n",
        );
    }
}
