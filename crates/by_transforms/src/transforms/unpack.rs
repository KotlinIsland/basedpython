//! AST pass: polyfills PEP 646 starred-type syntax in variadic parameter
//! annotations and inside subscript slices.
//!
//! `def f(*args: *tuple[int, ...])` → `def f(*args: Unpack[tuple[int, ...]])`
//! `tuple[*Ts]`                     → `tuple[Unpack[Ts]]`
//! `class Stack(Generic[*Ts]):`     → `class Stack(Generic[Unpack[Ts]]):`
//!
//! Also lowers basedpython's keyword-pack unpacking, which no python version accepts:
//!
//! `def f(**kwargs: **Kwargs)`      → `def f(**kwargs: Kwargs.kwargs)`

use std::cell::RefCell;

use ruff_python_ast::PythonVersion;
use ruff_python_ast::helpers::top_star_slice_elements;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext};
use crate::config::Config;

pub(crate) struct UnpackSyntax {
    config: Config,
}

impl UnpackSyntax {
    pub(crate) fn new(config: Config) -> Self {
        Self { config }
    }
}

impl AstPass for UnpackSyntax {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        // a keyword pack is a `ParamSpec` at runtime, and `**kwargs: *Pack` is invalid python at
        // every version, so this lowering is not version-gated like the `Unpack` polyfill below
        let mut keyword_pack = KeywordPackKwargs {
            edits: RefCell::new(Vec::new()),
        };
        for stmt in &module.body {
            keyword_pack.visit_stmt(stmt);
        }
        ctx.text_edits.extend(keyword_pack.edits.into_inner());

        if self.config.min_version >= PythonVersion::PY311 {
            return;
        }
        let mut state = State {
            edits: RefCell::new(Vec::new()),
            needs_import: false,
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        if state.needs_import {
            ctx.required_imports
                .push("from typing import Unpack".to_owned());
        }
        ctx.text_edits.extend(state.edits.into_inner());
    }
}

/// Lowers `**kwargs: *Pack` to the `ParamSpec` spelling `**kwargs: Pack.kwargs`.
///
/// The star is dropped and the suffix appended as two edits *around* the pack's name rather than
/// one replacement of the whole annotation, so the pep695 polyfill's typevar rename — which
/// rewrites that name in place — still lands.
struct KeywordPackKwargs {
    edits: RefCell<Vec<(TextRange, String)>>,
}

impl<'ast> Visitor<'ast> for KeywordPackKwargs {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt
            && let Some(kwarg) = &f.parameters.kwarg
            && let Some(Expr::Starred(starred)) = kwarg.annotation.as_deref()
        {
            let star = TextRange::new(starred.range().start(), starred.value.range().start());
            let end = starred.range().end();
            self.edits.borrow_mut().push((star, String::new()));
            self.edits
                .borrow_mut()
                .push((TextRange::new(end, end), ".kwargs".to_owned()));
        }
        walk_stmt(self, stmt);
    }
}

struct State {
    edits: RefCell<Vec<(TextRange, String)>>,
    needs_import: bool,
}

impl State {
    fn rewrite_subscript_starred(&mut self, starred: &ruff_python_ast::ExprStarred) {
        self.needs_import = true;
        let star_range = TextRange::new(starred.range().start(), starred.value.range().start());
        self.edits
            .borrow_mut()
            .push((star_range, "Unpack[".to_owned()));
        let end = starred.range().end();
        self.edits
            .borrow_mut()
            .push((TextRange::new(end, end), "]".to_owned()));
    }

    fn process_vararg_annotation(&mut self, ann: &Expr) {
        let Expr::Starred(starred) = ann else {
            return;
        };
        self.needs_import = true;
        let star_range = TextRange::new(ann.range().start(), starred.value.range().start());
        self.edits
            .borrow_mut()
            .push((star_range, "Unpack[".to_owned()));
        let end = ann.range().end();
        self.edits
            .borrow_mut()
            .push((TextRange::new(end, end), "]".to_owned()));
    }
}

impl<'ast> Visitor<'ast> for State {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt
            && let Some(vararg) = &f.parameters.vararg
            && let Some(ann) = &vararg.annotation
        {
            self.process_vararg_annotation(ann);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(s) = expr {
            if top_star_slice_elements(&s.slice).is_some() {
                walk_expr(self, expr);
                return;
            }
            match s.slice.as_ref() {
                Expr::Starred(st) => self.rewrite_subscript_starred(st),
                Expr::Tuple(t) if !t.has_parameter_shape() => {
                    for elt in &t.elts {
                        if let Expr::Starred(st) = elt {
                            self.rewrite_subscript_starred(st);
                        }
                    }
                }
                _ => {}
            }
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::config::PythonVersion;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn rewrites_starred_vararg_annotation() {
        check(
            "def f(*args: *tuple[int, ...]): ...\n",
            indoc! {"
                from typing_extensions import Unpack
                def f(*args: Unpack[tuple[int, ...]]): ...
            "},
        );
    }

    /// a keyword pack is a `ParamSpec` at runtime, so `**kwargs: **Kwargs` takes its `.kwargs`
    /// spelling. the pep695 polyfill's rename must still reach the pack's name
    #[test]
    fn rewrites_keyword_pack_kwargs_annotation() {
        check(
            "class A[**Kwargs]:\n    def __init__(self, **kwargs: **Kwargs) -> None: ...\n",
            indoc! {"
                from typing import ParamSpec, Generic
                _Kwargs = ParamSpec(\"_Kwargs\")
                class A(Generic[_Kwargs]):
                    def __init__(self, **kwargs: _Kwargs.kwargs) -> None: ...
            "},
        );
    }

    /// the lowering is not version-gated: no python version accepts `**kwargs: *Pack`
    #[test]
    fn rewrites_keyword_pack_kwargs_annotation_on_311() {
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        let output = transpile(
            "def f[**Kwargs](**kwargs: **Kwargs) -> None: ...\n",
            &config,
        )
        .expect("transpile failed");
        assert!(
            output.contains("**kwargs: _Kwargs.kwargs"),
            "unexpected output:\n{output}"
        );
    }

    #[test]
    fn no_rewrite_on_311() {
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        assert_eq!(
            transpile("def f(*args: *tuple[int, ...]): ...\n", &config).unwrap(),
            "def f(*args: *tuple[int, ...]): ...\n",
        );
    }

    #[test]
    fn nested_function() {
        check(
            indoc! {"
                class A:
                    def method(self, *args: *tuple[str, ...]): ...
            "},
            indoc! {"
                from typing_extensions import Unpack
                class A:
                    def method(self, *args: Unpack[tuple[str, ...]]): ...
            "},
        );
    }

    #[test]
    fn regular_arg_annotation_unchanged() {
        check("def f(x: int): ...\n", "def f(x: int): ...\n");
    }
}
