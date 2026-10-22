//! AST pass: lowers basedpython's `super` keyword sugar to standard python
//! `super(...)` calls.
//!
//! - `super.x`       → `super().x`
//! - `super[T].x`    → `super(<C>.__mro__[<C>.__mro__.index(<T>) - 1], <self>).x`
//!   where `<C>` is the enclosing class name and `<self>` is the first
//!   parameter of the function or lambda the keyword is written in — the
//!   receiver zero-argument `super()` reads there, and the one the checker types.
//!
//! either form is the keyword only as an attribute's base. anywhere else `super` and
//! `super[T]` are the name and its subscript, as python reads them.
//!
//! the keyword is always the builtin `super`, so in a module that binds the name `super`
//! itself — a parameter, a global — the call reads the builtin under a name of its own
//! ([`WrittenNames::builtin`]). zero-argument `super()` only works under the name `super`,
//! which is what has the compiler give the method its `__class__` cell, so the bare form
//! is then spelled out with the arguments it stands for: `__class__`, which itself has the
//! compiler make the cell, and the method's first parameter.

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
            builtin_super: self.written.builtin("super"),
            edits: RefCell::new(Vec::new()),
            class_stack: RefCell::new(Vec::new()),
            self_stack: RefCell::new(Vec::new()),
            errors: RefCell::new(Vec::new()),
        };
        let mut visitor = &state;
        for stmt in &module.body {
            visitor.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits.into_inner());
        ctx.errors.extend(state.errors.into_inner());
    }
}

struct State<'src> {
    written: WrittenNames<'src>,
    /// the name the builtin `super` is read by
    builtin_super: String,
    edits: RefCell<Vec<(TextRange, String)>>,
    class_stack: RefCell<Vec<String>>,
    /// the receiver of each function or lambda the walk is inside, innermost last: the name its
    /// first positional parameter is bound to, or `None` for one with no such parameter
    self_stack: RefCell<Vec<Option<String>>>,
    errors: RefCell<Vec<String>>,
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

/// the name a lambda's receiver is bound to in the python: its first positional parameter
fn first_lambda_param_name(
    lambda: &ruff_python_ast::ExprLambda,
    written: WrittenNames,
) -> Option<String> {
    let params = lambda.parameters.as_deref()?;
    params
        .posonlyargs
        .first()
        .or_else(|| params.args.first())
        .map(|p| crate::python_parameter_name(params, &p.parameter, written).to_string())
}

impl State<'_> {
    /// the receiver of the function or lambda the walk is inside, when it has one
    fn receiver(&self) -> Option<String> {
        self.self_stack.borrow().last().cloned().flatten()
    }

    /// the builtin `super` called with the arguments the bare keyword stands for
    fn bare(&self) -> String {
        if self.builtin_super == "super" {
            return "super()".to_owned();
        }
        match self.receiver() {
            Some(receiver) => format!("{}(__class__, {receiver})", self.builtin_super),
            // a function with no parameter to read the receiver from fails as zero-argument
            // `super()` does there, raising `RuntimeError`
            None => format!("{}()", self.builtin_super),
        }
    }
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
                self.self_stack
                    .borrow_mut()
                    .push(first_param_name(f, self.written));
                walk_stmt(self, stmt);
                self.self_stack.borrow_mut().pop();
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // a lambda reads `super`'s receiver from its own first parameter, as a `def` does
        if let Expr::Lambda(lambda) = expr {
            self.self_stack
                .borrow_mut()
                .push(first_lambda_param_name(lambda, self.written));
            walk_expr(self, expr);
            self.self_stack.borrow_mut().pop();
            return;
        }
        // the targeted keyword is the base of an attribute access, `super[T].x`. anywhere else
        // `super[T]` is the subscript of the name, as the checker reads it
        if let Expr::Attribute(attribute) = expr
            && let Expr::Subscript(s) = attribute.value.as_ref()
            && let Expr::Name(n) = s.value.as_ref()
            && n.id.as_str() == "super"
        {
            let cls = self
                .class_stack
                .borrow()
                .last()
                .cloned()
                .unwrap_or_default();
            let Some(self_name) = self.receiver() else {
                // written as an argument the receiver would have to be some other binding of
                // the name, an enclosing function's, which is not the one python reads
                self.errors.borrow_mut().push(format!(
                    "`{}` reads its receiver from the first parameter of the function it is \
                     written in, and it has none",
                    render_expr(&attribute.value)
                ));
                return;
            };
            let target_src = render_expr(&s.slice);
            let replacement = format!(
                "{}({cls}.__mro__[{cls}.__mro__.index({target_src}) - 1], {self_name})",
                self.builtin_super
            );
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
        // the keyword is the base of an attribute access, `super.x`, where the checker reads
        // it as one. read anywhere else, `super` is the name it is bound to, as it is there
        if let Expr::Attribute(attribute) = expr
            && let Expr::Name(n) = attribute.value.as_ref()
            && n.id.as_str() == "super"
            && matches!(n.ctx, ExprContext::Load)
        {
            self.edits.borrow_mut().push((n.range(), self.bare()));
            return;
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

    /// the keyword is the builtin wherever the module binds the name to something else
    #[test]
    fn a_module_binding_super_still_calls_the_builtin() {
        check(
            indoc! {"
                super = 5

                class C(A, B):
                    def f(this):
                        super.x
                        super[B].y
            "},
            indoc! {"
                from builtins import super as super2
                super = 5

                class C(A, B):
                    def f(this):
                        super2(__class__, this).x
                        super2(C.__mro__[C.__mro__.index(B) - 1], this).y
            "},
        );
    }

    /// read where it is not an attribute's base, `super` is the name the checker reads it as:
    /// the builtin class, or the parameter that binds the name
    #[test]
    fn super_read_as_a_value_is_the_name() {
        check(
            indoc! {"
                class A(B):
                    def f(self, super: B):
                        return super.g(super)
                    def h(self):
                        return super
            "},
            indoc! {"
                from builtins import super as super2
                class A(B):
                    def f(self, super: B):
                        return super2(__class__, self).g(super)
                    def h(self):
                        return super
            "},
        );
    }

    /// `super[T]` is the keyword only as an attribute's base. anywhere else it is the subscript
    /// of the name, as the checker reads it
    #[test]
    fn a_subscript_of_super_that_is_no_attribute_base_is_the_name() {
        unchanged(indoc! {"
            super = {1: \"one\"}

            class B(A):
                def f(self):
                    return super[1]
        "});
    }

    /// a lambda passes its own first parameter as the receiver, as zero-argument `super()`
    /// written in it would, not the method's
    #[test]
    fn a_lambda_passes_its_own_receiver() {
        check(
            indoc! {"
                super = 5

                class C(A, B):
                    def f(this):
                        return lambda other: super.x + super[B].y
            "},
            indoc! {"
                from builtins import super as super2
                super = 5

                class C(A, B):
                    def f(this):
                        return lambda other: super2(__class__, other).x + super2(C.__mro__[C.__mro__.index(B) - 1], other).y
            "},
        );
    }

    /// with no parameter there is no receiver to write, and an enclosing function's would be
    /// another one than python reads
    #[test]
    fn a_targeted_super_with_no_receiver_is_refused() {
        let error = transpile(
            indoc! {"
                class C(A, B):
                    def f(self):
                        return lambda: super[B].y
            "},
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            error.contains("reads its receiver from the first parameter"),
            "got: {error}"
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("class A:\n    def f(self):\n        super().x\n");
    }
}
