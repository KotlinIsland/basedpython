//! Runtime lowering for the postfix `!` force-unwrap operator.
//!
//! The parser models `expr!` as an `ExprUnaryOp` carrying `UnaryOp::Force`.
//! Each layer unwraps one level of optionality, raising on the absent value
//! (`None` for an optional, a `BaseException` for a result). This pass rewrites
//! `expr!` to `_force_unwrap(expr)` and injects the helper:
//!
//! ```python
//! def _force_unwrap(_v):
//!     if _v is None:
//!         raise RuntimeError("force-unwrap of absent value")
//!     if isinstance(_v, BaseException):
//!         raise RuntimeError("force-unwrap of absent value") from _v
//!     return _v
//! ```
//!
//! The rewrite is one template edit over the whole `expr!`, passing the operand
//! through: `_force_unwrap(` + everything but the trailing `!` + `)`. The
//! passthrough leaves the operand's bytes untouched — its own parentheses
//! included — so any sibling operator lowering inside it (`?.`, `??`, a grapheme
//! accessor) materializes within the wrap, and nested `expr!!` nests the
//! templates: `_force_unwrap(_force_unwrap(expr))`.
//!
//! One edit and not two. The wrap used to be an insertion at the operand's start
//! plus a `)` over the `!`, and a template edit on the operand — `s.first`
//! becoming `(Character(_by_graphemes(s)[0]) if s else None)` — separated them:
//! the insertion was absorbed into the template and re-emitted once per
//! passthrough of the receiver, while the `)` stayed outside, so `s.first!`
//! produced `_force_unwrap(s` twice and no matching close.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

struct ForceUnwrap {
    edits: Vec<(TextRange, Vec<Fragment>)>,
    used: bool,
}

impl ForceUnwrap {
    fn new() -> Self {
        Self {
            edits: Vec::new(),
            used: false,
        }
    }
}

impl<'ast> Visitor<'ast> for ForceUnwrap {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::UnaryOp(unary) = expr
            && unary.op == UnaryOp::Force
        {
            // everything but the trailing `!`, which is the operand as written —
            // a parenthesised operand keeps its own parens, and the bytes inside
            // are left for the sibling lowerings that key edits on them
            let operand =
                TextRange::new(expr.range().start(), expr.range().end() - TextSize::from(1));
            self.edits.push((
                expr.range(),
                vec![
                    Fragment::Lit("_force_unwrap(".to_owned()),
                    Fragment::Src(operand),
                    Fragment::Lit(")".to_owned()),
                ],
            ));
            self.used = true;
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct ForceUnwrapPass<'src> {
    source: &'src str,
}

impl<'src> ForceUnwrapPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for ForceUnwrapPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let _ = self.source;
        let mut inner = ForceUnwrap::new();
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if inner.used {
            // the helper unwraps the `Optional` value wrapper, so its runtime
            // class must be present (deduped if `Some`/`int??` already added it)
            ctx.runtime.insert(crate::runtime::OPTIONAL);
            ctx.runtime.insert(crate::runtime::FORCE_UNWRAP);
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    #[test]
    fn force_unwrap_of_parenthesised_lowered_operand() {
        // `!` on a parenthesised operand that itself lowers (`?.`, `??`) must
        // keep the operand's own parens balanced and let the inner lowering run
        let out = transpile("x = (a ?? b)!\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("x = _force_unwrap((a if a is not None else b))\n"),
            "got: {out}"
        );
        assert!(
            !out.contains('!') || out.contains("!r}"),
            "leftover !: {out}"
        );
    }

    #[test]
    fn single_force_unwrap() {
        check(
            "x = a!\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                def _force_unwrap(_v):
                    if isinstance(_v, Optional):
                        return _v.value
                    if _v is None:
                        raise RuntimeError(\"force-unwrap of absent value\")
                    if isinstance(_v, BaseException):
                        raise RuntimeError(\"force-unwrap of absent value\") from _v
                    return _v

                x = _force_unwrap(a)
            "},
        );
    }

    #[test]
    fn nested_force_unwrap() {
        check(
            "x = a!!\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                def _force_unwrap(_v):
                    if isinstance(_v, Optional):
                        return _v.value
                    if _v is None:
                        raise RuntimeError(\"force-unwrap of absent value\")
                    if isinstance(_v, BaseException):
                        raise RuntimeError(\"force-unwrap of absent value\") from _v
                    return _v

                x = _force_unwrap(_force_unwrap(a))
            "},
        );
    }

    #[test]
    fn force_unwrap_of_call() {
        check(
            "x = f()!\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                def _force_unwrap(_v):
                    if isinstance(_v, Optional):
                        return _v.value
                    if _v is None:
                        raise RuntimeError(\"force-unwrap of absent value\")
                    if isinstance(_v, BaseException):
                        raise RuntimeError(\"force-unwrap of absent value\") from _v
                    return _v

                x = _force_unwrap(f())
            "},
        );
    }

    #[test]
    fn helper_injected_once() {
        check(
            "x = a!\ny = b!\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                def _force_unwrap(_v):
                    if isinstance(_v, Optional):
                        return _v.value
                    if _v is None:
                        raise RuntimeError(\"force-unwrap of absent value\")
                    if isinstance(_v, BaseException):
                        raise RuntimeError(\"force-unwrap of absent value\") from _v
                    return _v

                x = _force_unwrap(a)
                y = _force_unwrap(b)
            "},
        );
    }

    /// the last line of the output, which is the statement the source's last line lowered to
    fn lowered(source: &str) -> String {
        let out = transpile(source, &Config::test_default())
            .unwrap_or_else(|error| panic!("{source:?}: {error}"));
        out.lines().last().expect("some output").to_owned()
    }

    /// `!` wraps whatever the operand lowered to, whether that lowering replaces the
    /// operand with something wider (a grapheme accessor, an optional chain) or leaves
    /// it alone. the wrap is one edit, so the operand's own rewrite materializes inside
    /// it exactly once
    #[test]
    fn force_unwrap_wraps_a_lowered_operand() {
        // a grapheme accessor, which re-emits its receiver twice: the reported shape
        // (`Expected ',', found 'else'`) was the `_force_unwrap(` landing on both
        assert_eq!(
            lowered("def go(s: str) -> None:\n    print(s.first!)\n"),
            "    print(_force_unwrap((Character(_by_graphemes(s)[0]) if s else None)))"
        );
        // an optional chain
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(a?.bit_length()!)\n"),
            "    print(_force_unwrap((None if a is None else a.bit_length())))"
        );
        // a subscript, a call and a comparison, none of which the operand rewrites
        assert_eq!(
            lowered("def go(xs: list[int?]) -> None:\n    print(xs[0]!)\n"),
            "    print(_force_unwrap(xs[0]))"
        );
        assert_eq!(
            lowered("def f() -> int?:\n    return 1\n\nprint(f()!)\n"),
            "print(_force_unwrap(f()))"
        );
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(a! > 1)\n"),
            "    print(_force_unwrap(a) > 1)"
        );
    }

    /// and inside an f-string replacement field, where the `!` may be bare: it is read as
    /// the start of a conversion (`!r`) only where a conversion could follow it
    #[test]
    fn force_unwrap_inside_an_f_string() {
        assert_eq!(
            lowered("def go(s: str) -> None:\n    print(f\"{(s.first!)}\")\n"),
            "    print(f\"{(_force_unwrap((Character(_by_graphemes(s)[0]) if s else None)))}\")"
        );
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{a!}\")\n"),
            "    print(f\"{_force_unwrap(a)}\")"
        );
        // before the `:` of a format spec, and before the conversion's own `!`
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{a!:>4}\")\n"),
            "    print(f\"{_force_unwrap(a):>4}\")"
        );
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{a!!r}\")\n"),
            "    print(f\"{_force_unwrap(a)!r}\")"
        );
        // a nested f-string, and a field inside a format spec
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{f'{a!}'}\")\n"),
            "    print(f\"{f'{_force_unwrap(a)}'}\")"
        );
        assert_eq!(
            lowered("def go(a: int?, b: int) -> None:\n    print(f\"{b:{a!}}\")\n"),
            "    print(f\"{b:{_force_unwrap(a)}}\")"
        );
        // the conversion the `!` is usually reaching for is untouched, and a `!` glued to
        // `=` is still the comparison operator
        assert_eq!(
            lowered(
                "def go(a: int, b: int) -> None:\n    print(f\"{a!r} {a!s:>10} {a=!r} {a!=b}\")\n"
            ),
            "    print(f\"{a!r} {a!s:>10} {a=!r} {a!=b}\")"
        );
    }

    #[test]
    fn plain_python_unchanged() {
        unchanged("x = a\n");
    }
}
