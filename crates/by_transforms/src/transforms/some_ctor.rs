//! Runtime lowering for the `Some(...)` optional constructor.
//!
//! `Some` is the present-case constructor for a wrapped optional. It lowers to
//! the runtime `Optional` wrapper class (see [`wrapped_runtime`]), so `Some(x)`
//! becomes `Optional(x)`. The class is injected as a polyfill when any `Some`
//! reference is rewritten.
//!
//! The rewrite replaces only the `Some` identifier (a narrow edit), so call
//! arguments and surrounding context are left for sibling passes to lower.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprContext, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::wrapped_runtime::OPTIONAL_RUNTIME;
use crate::type_info::TypeInfo;

struct SomeCtor {
    edits: Vec<(TextRange, String)>,
    used: bool,
}

impl SomeCtor {
    fn new() -> Self {
        Self {
            edits: Vec::new(),
            used: false,
        }
    }
}

impl<'ast> Visitor<'ast> for SomeCtor {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(name) = expr
            && name.ctx == ExprContext::Load
            && name.id.as_str() == "Some"
        {
            self.edits.push((name.range(), "Optional".to_owned()));
            self.used = true;
        }
        walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }
}

pub(crate) struct SomeCtorPass;

impl SomeCtorPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for SomeCtorPass {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = SomeCtor::new();
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if inner.used {
            ctx.required_imports.push(OPTIONAL_RUNTIME.to_owned());
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
    fn some_of_none() {
        check(
            "a = Some(None)\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                a = Optional(None)
            "},
        );
    }

    #[test]
    fn some_of_value() {
        check(
            "a = Some(1)\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                a = Optional(1)
            "},
        );
    }

    #[test]
    fn some_as_callable_argument() {
        check(
            "xs = list(map(Some, ys))\n",
            indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                xs = list(map(Optional, ys))
            "},
        );
    }

    #[test]
    fn plain_python_unchanged() {
        unchanged("a = foo(None)\n");
    }
}
