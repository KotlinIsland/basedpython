//! context parameters (basedpython).
//!
//! a `context` parameter is filled implicitly at call sites from the `context`
//! declarations in scope, resolved by assignability. ty performs the
//! resolution (`types::context_params`); this pass lowers the three surface
//! forms to plain python:
//!
//! ```by
//! def f(a: int, context b: str): ...
//!
//! context s1 = "asdf"
//! f(2)
//! ```
//!
//! →
//!
//! ```python
//! def f(a: int, b: str): ...
//!
//! s1 = "asdf"
//! f(2, b=s1)
//! ```
//!
//! the lowering is intentionally lossy: the emitted python is an ordinary
//! function taking ordinary arguments, with every call site explicit. there
//! is nothing to detect in the output, so no reverse transform exists — a
//! round-trip degrades context calls to the explicit form, which is also
//! valid basedpython
//!
//! a call that fails resolution (missing or ambiguous — both check errors)
//! gets no injection: the emitted call raises `TypeError` at runtime, which
//! matches the source not having type-checked

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct ContextParamsPass<'src> {
    source: &'src str,
}

impl<'src> ContextParamsPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for ContextParamsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut lowerer = ContextLowerer {
            source: self.source,
            types,
            edits: Vec::new(),
        };
        for stmt in stmts {
            lowerer.visit_stmt(stmt);
        }
        ctx.text_edits.extend(lowerer.edits);
    }
}

struct ContextLowerer<'src, 'ti> {
    source: &'src str,
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl ContextLowerer<'_, '_> {
    /// strip the `context ` prefix from each marked parameter. the prefix is
    /// exactly the source between the parameter start and its name
    fn strip_parameter_prefixes(&mut self, parameters: &ast::Parameters) {
        for parameter in parameters
            .iter()
            .map(ast::AnyParameterRef::as_parameter)
            .filter(|parameter| parameter.is_context)
        {
            self.edits.push((
                TextRange::new(parameter.range().start(), parameter.name.range().start()),
                String::new(),
            ));
        }
    }

    /// `context s1 [: T] = v` → `s1 [: T] = v`: erase from the statement
    /// start to the target name
    fn lower_declaration(&mut self, decl: &ast::StmtAnnAssign) {
        let is_marker = match &*decl.annotation {
            Expr::Name(name) => name.id == "__context__",
            Expr::Subscript(subscript) => {
                matches!(&*subscript.value, Expr::Name(name) if name.id == "__context__")
            }
            _ => false,
        };
        if is_marker && decl.target.is_name_expr() {
            self.edits.push((
                TextRange::new(decl.range().start(), decl.target.range().start()),
                String::new(),
            ));
        }
    }

    /// append the resolved implicit arguments before the call's closing paren
    fn lower_call(&mut self, call: &ast::ExprCall) {
        // an extension member's call is re-emitted whole by the extension
        // lowering — receiver first, then the arguments — so the separator this
        // reads off the source parens is not the one the output needs. that
        // lowering fills the `context` arguments itself
        if let Expr::Attribute(attr) = call.func.as_ref()
            && attr.ctx.is_load()
            && self.types.extension_attribute_info(attr).is_some()
        {
            return;
        }
        let implicit = self.types.implicit_context_arguments(call);
        if implicit.is_empty() {
            return;
        }
        let arguments = implicit
            .iter()
            .map(|(parameter, variable)| format!("{parameter}={variable}"))
            .collect::<Vec<_>>()
            .join(", ");
        let parens = call.arguments.range();
        let inner = &self.source
            [usize::from(parens.start()) + 1..usize::from(parens.end()).saturating_sub(1)];
        let separator = match inner.trim_end().as_bytes().last() {
            None => "",
            Some(b',') => " ",
            Some(_) => ", ",
        };
        self.edits.push((
            TextRange::empty(parens.end() - TextSize::from(1)),
            format!("{separator}{arguments}"),
        ));
    }
}

impl<'ast> Visitor<'ast> for ContextLowerer<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => self.strip_parameter_prefixes(&function.parameters),
            Stmt::AnnAssign(decl) => self.lower_declaration(decl),
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Call(call) => self.lower_call(call),
            Expr::Lambda(lambda) => {
                if let Some(parameters) = lambda.parameters.as_deref() {
                    self.strip_parameter_prefixes(parameters);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::lazify_expected;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        let out = transpile(input, &Config::test_default()).unwrap();
        assert_eq!(out, lazify_expected(expected));
    }

    #[test]
    fn call_receives_implicit_argument() {
        check(
            indoc! {r#"
                def f(a: int, context b: str): ...

                context s1 = "asdf"
                f(2)
            "#},
            indoc! {r#"
                def f(a: int, b: str): ...

                s1 = "asdf"
                f(2, b=s1)
            "#},
        );
    }

    #[test]
    fn explicit_argument_wins() {
        check(
            indoc! {r#"
                def f(a: int, context b: str): ...

                context s1 = "asdf"
                f(2, b="explicit")
                f(2, "positional")
            "#},
            indoc! {r#"
                def f(a: int, b: str): ...

                s1 = "asdf"
                f(2, b="explicit")
                f(2, "positional")
            "#},
        );
    }

    #[test]
    fn typed_declaration_keeps_annotation() {
        check(
            indoc! {r#"
                def f(context b: str): ...

                context s1: str = "asdf"
                f()
            "#},
            indoc! {r#"
                def f(b: str): ...

                s1: str = "asdf"
                f(b=s1)
            "#},
        );
    }

    #[test]
    fn resolution_picks_by_assignability() {
        check(
            indoc! {r#"
                def f(context b: str, context n: int): ...

                context s1 = "asdf"
                context count = 3
                f()
            "#},
            indoc! {r#"
                def f(b: str, n: int): ...

                s1 = "asdf"
                count = 3
                f(b=s1, n=count)
            "#},
        );
    }

    #[test]
    fn context_parameter_propagates_through_body() {
        check(
            indoc! {r#"
                def f(context b: str): ...

                def g(x: int, context b: str):
                    f()
            "#},
            indoc! {r#"
                def f(b: str): ...

                def g(x: int, b: str):
                    f(b=b)
            "#},
        );
    }

    #[test]
    fn inner_scope_shadows_outer() {
        check(
            indoc! {r#"
                def f(context b: str): ...

                context outer = "module"

                def g():
                    context inner = "local"
                    f()
            "#},
            indoc! {r#"
                def f(b: str): ...

                outer = "module"

                def g():
                    inner = "local"
                    f(b=inner)
            "#},
        );
    }

    #[test]
    fn keyword_only_context_parameter() {
        check(
            indoc! {r#"
                def f(a: int, *, context b: str): ...

                context s1 = "asdf"
                f(1)
            "#},
            indoc! {r#"
                def f(a: int, *, b: str): ...

                s1 = "asdf"
                f(1, b=s1)
            "#},
        );
    }

    #[test]
    fn trailing_lambda_it_fills_a_context_parameter() {
        check(
            indoc! {r#"
                def f(context b: str): ...
                def each(fn: (str) -> None): ...

                each:
                    f()
            "#},
            indoc! {r#"
                from typing import Callable
                def f(b: str): ...
                def each(fn: Callable[[str], None]): ...

                def _trailing_lambda_0(it=None):
                    f(b=it)
                each(fn=_trailing_lambda_0)
            "#},
        );
    }

    #[test]
    fn trailing_lambda_receiver_fills_a_context_parameter() {
        // the block's receiver is spelled `self` in the source but has a name of
        // its own in the lowering, which is what the injected argument must use
        check(
            indoc! {r#"
                def f(context b: str): ...
                def against(fn: str.() -> None): ...

                against:
                    f()
            "#},
            indoc! {r#"
                from typing import Callable
                def f(b: str): ...
                def against(fn: Callable[[str], None]): ...

                def _trailing_lambda_0(_by_self=None, it=None):
                    f(b=_by_self)
                against(fn=_trailing_lambda_0)
            "#},
        );
    }

    #[test]
    fn unresolved_call_left_alone() {
        // no declaration in scope: check errors, the lowering injects nothing
        check(
            indoc! {r#"
                def f(context b: str): ...

                f()
            "#},
            indoc! {r#"
                def f(b: str): ...

                f()
            "#},
        );
    }
}
