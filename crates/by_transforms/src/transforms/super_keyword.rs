//! AST pass: lowers basedpython's `super` keyword sugar to standard python
//! `super(...)` calls.
//!
//! - `super.x`       → `super().x`
//! - `super[T].x`    → `super(<C>.__mro__[<C>.__mro__.index(<T>) - 1], <self>).x`
//!   where `<C>` is the enclosing class name and `<self>` is the first
//!   parameter name of the enclosing method.

use std::cell::RefCell;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprContext, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext, render_expr};
use super::repeated_underscore::WrittenNames;

pub(crate) struct SuperKeyword<'src> {
    written: WrittenNames<'src>,
}

impl<'src> SuperKeyword<'src> {
    pub(crate) fn new(written: WrittenNames<'src>) -> Self {
        Self { written }
    }
}

impl AstPass for SuperKeyword<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let state = State {
            written: self.written,
            edits: RefCell::new(Vec::new()),
            class_stack: RefCell::new(Vec::new()),
            self_stack: RefCell::new(Vec::new()),
        };
        let mut visitor = &state;
        for stmt in &module.body {
            visitor.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits.into_inner());
    }
}

struct State<'src> {
    written: WrittenNames<'src>,
    edits: RefCell<Vec<(TextRange, String)>>,
    class_stack: RefCell<Vec<String>>,
    self_stack: RefCell<Vec<String>>,
}

/// the name the receiver is bound to in the python, which a repeated `_` may have changed
fn first_param_name(
    func: &ruff_python_ast::StmtFunctionDef,
    written: WrittenNames,
) -> Option<String> {
    let params = &func.parameters;
    params
        .posonlyargs
        .first()
        .or_else(|| params.args.first())
        .map(|p| crate::python_parameter_name(params, &p.parameter, written).to_string())
}

impl<'ast> Visitor<'ast> for &State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(c) => {
                self.class_stack
                    .borrow_mut()
                    .push(c.name.id.as_str().to_owned());
                walk_stmt(self, stmt);
                self.class_stack.borrow_mut().pop();
            }
            Stmt::FunctionDef(f) => {
                let pushed = first_param_name(f, self.written);
                if let Some(name) = &pushed {
                    self.self_stack.borrow_mut().push(name.clone());
                }
                walk_stmt(self, stmt);
                if pushed.is_some() {
                    self.self_stack.borrow_mut().pop();
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(s) = expr
            && let Expr::Name(n) = s.value.as_ref()
            && n.id.as_str() == "super"
        {
            let cls = self
                .class_stack
                .borrow()
                .last()
                .cloned()
                .unwrap_or_default();
            let self_name = self
                .self_stack
                .borrow()
                .last()
                .cloned()
                .unwrap_or_else(|| "self".to_owned());
            let target_src = render_expr(&s.slice);
            let replacement =
                format!("super({cls}.__mro__[{cls}.__mro__.index({target_src}) - 1], {self_name})");
            self.edits.borrow_mut().push((s.range(), replacement));
            return;
        }
        if let Expr::Call(call) = expr
            && let Expr::Name(n) = call.func.as_ref()
            && n.id.as_str() == "super"
        {
            self.visit_arguments(&call.arguments);
            return;
        }
        if let Expr::Name(n) = expr
            && n.id.as_str() == "super"
            && matches!(n.ctx, ExprContext::Load)
        {
            self.edits
                .borrow_mut()
                .push((n.range(), "super()".to_owned()));
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn bare_super_attr() {
        check(
            indoc! {"
                class A:
                    def f(self):
                        super.x
            "},
            indoc! {"
                class A:
                    def f(self):
                        super().x
            "},
        );
    }

    #[test]
    fn bare_super_call() {
        check(
            indoc! {"
                class A:
                    def f(self):
                        super.f()
            "},
            indoc! {"
                class A:
                    def f(self):
                        super().f()
            "},
        );
    }

    #[test]
    fn super_subscript_attr() {
        check(
            indoc! {"
                class A:
                    def f(self): ...

                class B:
                    def f(self): ...

                class C(A, B):
                    def f(self):
                        super[B].x
            "},
            indoc! {"
                class A:
                    def f(self): ...

                class B:
                    def f(self): ...

                class C(A, B):
                    def f(self):
                        super(C.__mro__[C.__mro__.index(B) - 1], self).x
            "},
        );
    }

    #[test]
    fn super_subscript_call() {
        check(
            indoc! {"
                class C(A, B):
                    def f(self):
                        super[B].f()
            "},
            indoc! {"
                class C(A, B):
                    def f(self):
                        super(C.__mro__[C.__mro__.index(B) - 1], self).f()
            "},
        );
    }

    #[test]
    fn super_uses_first_param_name() {
        check(
            indoc! {"
                class C(A):
                    def f(this):
                        super[A].x
            "},
            indoc! {"
                class C(A):
                    def f(this):
                        super(C.__mro__[C.__mro__.index(A) - 1], this).x
            "},
        );
    }

    #[test]
    fn nested_class_uses_inner_class() {
        check(
            indoc! {"
                class Outer:
                    class Inner(Base):
                        def f(self):
                            super.x
            "},
            indoc! {"
                class Outer:
                    class Inner(Base):
                        def f(self):
                            super().x
            "},
        );
    }

    #[test]
    fn super_as_assignment_target_unchanged() {
        unchanged(indoc! {"
            class Class:
                super: list[str] | None

                def __init__(self, super: list[str] | None) -> None:
                    self.super
        "});
    }

    #[test]
    fn python_unchanged() {
        unchanged("class A:\n    def f(self):\n        super().x\n");
    }
}
