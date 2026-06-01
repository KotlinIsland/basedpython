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
//! The rewrite uses narrow edits — a `_force_unwrap(` insertion at the operand
//! start and a `)` replacement of the trailing `!` — so the operand's bytes
//! are left untouched and any sibling operator lowering inside it (e.g. `?.`
//! or `??`) still applies. Nested `expr!!` composes: the two insertions at the
//! same offset concatenate, yielding `_force_unwrap(_force_unwrap(expr))`.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::wrapped_runtime::OPTIONAL_RUNTIME;
use crate::type_info::TypeInfo;

// peels one absent layer. a present wrapped value (`Some(x)` → `Optional(x)`)
// yields its inner `.value`; a plain `T | None` yields the value or raises on
// `None`; a result-like `T | E` raises on a `BaseException` value, chaining it
// as `__cause__`. referencing `Optional` means the runtime class is co-injected
// below.
const FORCE_HELPER: &str = "\
def _force_unwrap(_v):
    if isinstance(_v, Optional):
        return _v.value
    if _v is None:
        raise RuntimeError(\"force-unwrap of absent value\")
    if isinstance(_v, BaseException):
        raise RuntimeError(\"force-unwrap of absent value\") from _v
    return _v
";

struct ForceUnwrap {
    edits: Vec<(TextRange, String)>,
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
            // `_force_unwrap(` before the operand (a zero-width insertion; nested
            // forces at the same offset concatenate in push order)
            self.edits.push((
                TextRange::empty(expr.range().start()),
                "_force_unwrap(".to_owned(),
            ));
            // `)` in place of the trailing `!` only — replacing the whole gap
            // between the operand and `!` would swallow a parenthesised
            // operand's own closing paren (`(a?.v)!` → unbalanced), and the
            // operand's interior bytes are left for sibling lowerings (`?.`/`??`)
            self.edits.push((
                TextRange::new(expr.range().end() - TextSize::from(1), expr.range().end()),
                ")".to_owned(),
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
            ctx.required_imports.push(OPTIONAL_RUNTIME.to_owned());
            ctx.required_imports.push(FORCE_HELPER.to_owned());
        }
        ctx.text_edits.extend(inner.edits);
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

    #[test]
    fn plain_python_unchanged() {
        unchanged("x = a\n");
    }
}
