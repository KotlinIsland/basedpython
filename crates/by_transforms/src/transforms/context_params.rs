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
//!
//! a decoration is the same bargain. `@deco` is a call, but the source writes no
//! argument list for it, so there is nowhere to append the resolved argument —
//! the decoration is emitted as written, and checking reports the parameter
//! (`missing-context-argument`) rather than letting it quietly take its default.
//! a decoration written in the factory form (`@deco(...)`) is an ordinary call
//! expression and is filled like any other
//!
//! a value that would come from a `_` parameter its function repeats is a
//! transpile error instead. python binds only one of those parameters to `_`,
//! and which one is not decided, so there is no name to write for it. so is a
//! value for a `_` parameter the callee repeats, since which of those a keyword
//! `_` names is not decided either

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
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::ContextParams)
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut lowerer = ContextLowerer {
            source: self.source,
            types,
            edits: Vec::new(),
            errors: Vec::new(),
        };
        for stmt in stmts {
            lowerer.visit_stmt(stmt);
        }
        ctx.text_edits.extend(lowerer.edits);
        ctx.errors.extend(lowerer.errors);
    }
}

struct ContextLowerer<'src, 'ti> {
    source: &'src str,
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
    errors: Vec<String>,
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
        let implicit = match self.types.implicit_context_arguments(call) {
            Ok(implicit) if implicit.is_empty() => return,
            Ok(implicit) => implicit,
            Err(error) => {
                self.errors.push(error);
                return;
            }
        };
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
        // a `context` declaration needs no lowering of its own: it is an ordinary modifier
        // declaration, and the `modifiers` pass already erases the whole keyword prefix
        // ahead of the name it binds
        if let Stmt::FunctionDef(function) = stmt {
            self.strip_parameter_prefixes(&function.parameters);
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
    fn a_composed_declaration_lowers_as_the_declaration_it_is() {
        // `context` is a prefix on a declaration, not a form of its own, so the rest of
        // the chain decides what the output looks like — `let` still lowers to `Final`
        check(
            indoc! {r#"
                def f(context b: str): ...

                context let s1: str = "asdf"
                f()
            "#},
            indoc! {r#"
                from typing import Final
                def f(b: str): ...

                s1: Final[str] = "asdf"
                f(b=s1)
            "#},
        );
    }

    /// a decoration has no argument list to write the implicit argument in, so it is
    /// emitted exactly as written and checking reports the parameter. injecting here would
    /// have to wrap the decorator expression, which changes what `@` evaluates to
    #[test]
    fn a_decoration_is_left_as_written() {
        check(
            indoc! {r#"
                def deco(fn: (...) -> object, context b: str = "d") -> object:
                    return fn

                context s1 = "asdf"

                @deco
                def g(): ...
            "#},
            indoc! {r#"
                from typing import Callable
                def deco(fn: Callable[..., object], b: str = "d") -> object:
                    return fn

                s1 = "asdf"

                @deco
                def g(): ...
            "#},
        );
    }

    /// and the factory form of the same decoration is an ordinary call expression, which is
    /// filled like any other
    #[test]
    fn a_decoration_written_as_a_call_is_filled() {
        check(
            indoc! {r#"
                def deco(context b: str = "d") -> (((...) -> object)) -> object:
                    return lambda fn: fn

                context s1 = "asdf"

                @deco()
                def g(): ...
            "#},
            indoc! {r#"
                from typing import Callable
                def deco(b: str = "d") -> Callable[[Callable[..., object]], object]:
                    return lambda fn: fn

                s1 = "asdf"

                @deco(b=s1)
                def g(): ...
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
    fn block_carrying_call_receives_implicit_argument() {
        // a call that carries a trailing block is still a call: its `context`
        // parameter is filled like any other, ahead of the block's own keyword
        check(
            indoc! {r#"
                def Card(title: str, context theme: str, once content: () -> None):
                    content()

                context theme = "dark"

                Card("x"):
                    pass
            "#},
            indoc! {r#"
                from typing import Callable
                def Card(title: str, theme: str, content: Callable[[], None]):
                    content()

                theme = "dark"

                def _trailing_lambda_0(it=None):
                    pass
                Card("x", theme=theme, content=_trailing_lambda_0)
            "#},
        );
    }

    #[test]
    fn block_carrying_call_with_no_written_arguments_separates_them() {
        // the block's keyword and the implicit `context` argument are spliced
        // in at the same point — before the closing paren — and each decides
        // its own separator from the source, which has nothing between the
        // parens to separate from. without one of them accounting for the
        // other the two run together as `Card(theme=themecontent=...)`, which
        // is not python
        check(
            indoc! {r#"
                def Card(context theme: str, once content: () -> None):
                    content()

                context theme = "dark"

                Card():
                    pass
            "#},
            indoc! {r#"
                from typing import Callable
                def Card(theme: str, content: Callable[[], None]):
                    content()

                theme = "dark"

                def _trailing_lambda_0(it=None):
                    pass
                Card(theme=theme, content=_trailing_lambda_0)
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

    #[test]
    fn a_single_underscore_parameter_fills_a_context_parameter() {
        check(
            indoc! {r#"
                def show(context label: str) -> str:
                    return label

                def relay(context _: str) -> str:
                    return show()
            "#},
            indoc! {r#"
                def show(label: str) -> str:
                    return label

                def relay(_: str) -> str:
                    return show(label=_)
            "#},
        );
    }

    #[test]
    fn a_repeated_underscore_parameter_is_refused_as_a_context_argument() {
        // python binds only one of the parameters to `_`, and which one a read of it
        // means is not decided. writing `label=_` answered the `int` argument where a
        // `str` was declared
        let source = indoc! {r#"
            def show(context label: str) -> str:
                return label

            def relay(context _: int, context _: str) -> str:
                return show()
        "#};
        let error = transpile(source, &Config::test_default()).unwrap_err();
        assert_eq!(
            error,
            "a repeated `_` parameter cannot supply the context argument `label`"
        );
    }

    #[test]
    fn a_repeated_underscore_is_refused_for_each_argument_of_one_call() {
        // two arguments from repeated `_` parameters wrote `_=` twice, which python
        // refuses to import
        let source = indoc! {r#"
            def show(context a: int, context b: str) -> str:
                return b

            def relay(context _: int, context _: str) -> str:
                return show()
        "#};
        let error = transpile(source, &Config::test_default()).unwrap_err();
        assert_eq!(
            error,
            "a repeated `_` parameter cannot supply the context argument `a`"
        );
    }

    #[test]
    fn a_repeated_underscore_parameter_of_the_callee_is_refused_an_implicit_argument() {
        // each value was written as `_=`, twice, which python refuses to import
        let source = indoc! {r#"
            def show(context _: int, context _: str) -> str:
                return "ok"

            context n = 1
            context s = "a"
            show()
        "#};
        let error = transpile(source, &Config::test_default()).unwrap_err();
        assert_eq!(
            error,
            "a context argument cannot be supplied implicitly for a repeated `_` parameter"
        );
    }

    #[test]
    fn a_keyword_underscore_does_not_fill_a_repeated_underscore_parameter() {
        // the keyword was taken to match both parameters named `_`, so nothing was
        // written for the other one and the call raised `TypeError`
        let source = indoc! {r#"
            def show(context _: int, context _: str) -> str:
                return "ok"

            context s = "a"
            show(_=1)
        "#};
        let error = transpile(source, &Config::test_default()).unwrap_err();
        assert_eq!(
            error,
            "a context argument cannot be supplied implicitly for a repeated `_` parameter"
        );
    }

    #[test]
    fn a_context_underscore_repeating_an_ordinary_parameter_is_refused_an_implicit_argument() {
        let source = indoc! {r#"
            def show(_: int, context _: str) -> str:
                return "ok"

            context s = "a"
            show(1)
        "#};
        let error = transpile(source, &Config::test_default()).unwrap_err();
        assert_eq!(
            error,
            "a context argument cannot be supplied implicitly for a repeated `_` parameter"
        );
    }

    #[test]
    fn overloads_that_agree_on_a_keyword_only_parameter_receive_the_implicit_argument() {
        // which overload a call selects is decided by its arguments, so an argument is
        // written only where it is the right one for every overload at once
        check(
            indoc! {r#"
                from typing import overload

                @overload
                def f(a: int, *, context b: str) -> int: ...
                @overload
                def f(a: str, *, context b: str) -> str: ...
                def f(a: int | str, *, context b: str) -> int | str:
                    return a

                context s1 = "asdf"
                f(2)
            "#},
            indoc! {r#"
                from typing import overload

                @overload
                def f(a: int, *, b: str) -> int: ...
                @overload
                def f(a: str, *, b: str) -> str: ...
                def f(a: int | str, *, b: str) -> int | str:
                    return a

                s1 = "asdf"
                f(2, b=s1)
            "#},
        );
    }

    #[test]
    fn overloads_that_disagree_on_a_parameter_receive_nothing() {
        // the name means a different thing in each, so no one argument is right whichever
        // overload the call selects
        check(
            indoc! {r#"
                from typing import overload

                @overload
                def f(a: int, *, context b: str = "d") -> int: ...
                @overload
                def f(a: str, *, context b: int = 0) -> str: ...
                def f(a: int | str, *, context b: str | int = "d") -> int | str:
                    return a

                context s1 = "asdf"
                f(2)
            "#},
            indoc! {r#"
                from typing import overload

                @overload
                def f(a: int, *, b: str = "d") -> int: ...
                @overload
                def f(a: str, *, b: int = 0) -> str: ...
                def f(a: int | str, *, b: str | int = "d") -> int | str:
                    return a

                s1 = "asdf"
                f(2)
            "#},
        );
    }

    #[test]
    fn repeated_underscore_parameters_passed_positionally_need_no_implicit_argument() {
        check(
            indoc! {r#"
                def show(context _: int, context _: str) -> str:
                    return "ok"

                context n = 1
                show(2, "b")
            "#},
            indoc! {r#"
                def show(_: int, _2: str, /) -> str:
                    return "ok"

                n = 1
                show(2, "b")
            "#},
        );
    }
}
