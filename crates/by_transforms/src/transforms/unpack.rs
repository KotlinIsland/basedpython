//! AST pass: polyfills PEP 646 starred-type syntax in variadic parameter
//! annotations and inside subscript slices.
//!
//! `def f(*args: *tuple[int, ...])` → `def f(*args: Unpack[tuple[int, ...]])`
//! `tuple[*Ts]`                     → `tuple[Unpack[Ts]]`
//! `class Stack(Generic[*Ts]):`     → `class Stack(Generic[Unpack[Ts]]):`
//!
//! Also lowers basedpython's pack forwarding, which no python version accepts:
//!
//! `def f(**kwargs: **Kwargs)`      → `def f(**kwargs: Kwargs.kwargs)`
//! `def f(*args: *P, **kwargs: **P)` → `def f(*args: P.args, **kwargs: P.kwargs)`

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
        // a parameter pack is a `ParamSpec` at runtime, and neither `**kwargs: **Pack` nor
        // `*args: *P` is valid python at any version, so this lowering is not version-gated
        // like the `Unpack` polyfill below
        let mut pack = PackForwarding {
            edits: RefCell::new(Vec::new()),
            lowered_varargs: Vec::new(),
        };
        for stmt in &module.body {
            pack.visit_stmt(stmt);
        }
        ctx.text_edits.extend(pack.edits.into_inner());

        if self.config.min_version >= PythonVersion::PY311 {
            return;
        }
        let mut state = State {
            edits: RefCell::new(Vec::new()),
            needs_import: false,
            lowered_varargs: pack.lowered_varargs,
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

/// Lowers a forwarded parameter pack to the `ParamSpec` spelling: `**kwargs: **Pack` to
/// `**kwargs: Pack.kwargs`, and the `*args: *P` that pairs with it to `*args: P.args`.
///
/// The stars are dropped and the suffix appended as two edits *around* the pack's name rather than
/// one replacement of the whole annotation, so the pep695 polyfill's typevar rename — which
/// rewrites that name in place — still lands.
///
/// A single-starred `*args` annotation is a `TypeVarTuple` unpack unless the same name is
/// double-starred by the `**kwargs` of the same signature: a `ParamSpec` may only be forwarded as
/// the pair, which is what makes the two spellings tell apart without asking for a type.
struct PackForwarding {
    edits: RefCell<Vec<(TextRange, String)>>,
    /// ranges of the `*args` annotations lowered here, which the `Unpack` polyfill must leave
    /// alone
    lowered_varargs: Vec<TextRange>,
}

impl PackForwarding {
    /// drop the stars off `starred` and append `suffix` to the name they applied to
    fn rewrite(&self, starred: &ruff_python_ast::ExprStarred, suffix: &str) {
        let stars = TextRange::new(starred.range().start(), starred.value.range().start());
        let end = starred.range().end();
        self.edits.borrow_mut().push((stars, String::new()));
        self.edits
            .borrow_mut()
            .push((TextRange::new(end, end), suffix.to_owned()));
    }
}

/// the name a `**Pack` annotation forwards, if that is what `annotation` is
fn double_starred_pack_name(annotation: Option<&Expr>) -> Option<&str> {
    let Some(Expr::Starred(outer)) = annotation else {
        return None;
    };
    let Expr::Starred(inner) = outer.value.as_ref() else {
        return None;
    };
    Some(&inner.value.as_name_expr()?.id)
}

impl<'ast> Visitor<'ast> for PackForwarding {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            let pack = double_starred_pack_name(
                f.parameters
                    .kwarg
                    .as_ref()
                    .and_then(|kwarg| kwarg.annotation.as_deref()),
            );
            if let Some(kwarg) = &f.parameters.kwarg
                && let Some(Expr::Starred(starred)) = kwarg.annotation.as_deref()
            {
                self.rewrite(starred, ".kwargs");
            }
            if let Some(pack) = pack
                && let Some(vararg) = &f.parameters.vararg
                && let Some(Expr::Starred(starred)) = vararg.annotation.as_deref()
                && starred.value.as_name_expr().is_some_and(|n| n.id == pack)
            {
                self.rewrite(starred, ".args");
                self.lowered_varargs.push(starred.range());
            }
        }
        walk_stmt(self, stmt);
    }
}

struct State {
    edits: RefCell<Vec<(TextRange, String)>>,
    needs_import: bool,
    lowered_varargs: Vec<TextRange>,
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
        // a forwarded `ParamSpec` already became `P.args`, which is not an unpack
        if self.lowered_varargs.contains(&starred.range()) {
            return;
        }
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

    /// a `ParamSpec` is forwarded as the `*args` / `**kwargs` pair, which takes the runtime
    /// `.args` / `.kwargs` spelling
    #[test]
    fn rewrites_forwarded_paramspec_pair() {
        check(
            "def f[P: (*: *, **: *)](*args: *P, **kwargs: **P) -> None: ...\n",
            indoc! {"
                from typing import ParamSpec
                _P = ParamSpec(\"_P\")
                def f(*args: _P.args, **kwargs: _P.kwargs) -> None: ...
            "},
        );
    }

    /// the pair is what tells a forwarded `ParamSpec` from a `TypeVarTuple` unpack: on its own,
    /// `*args: *Ts` is still the unpack
    #[test]
    fn unpaired_starred_vararg_stays_an_unpack() {
        check(
            "def f[*Ts](*args: *Ts) -> None: ...\n",
            indoc! {"
                from typing_extensions import TypeVarTuple, Unpack
                _Ts = TypeVarTuple(\"_Ts\")
                def f(*args: Unpack[_Ts]) -> None: ...
            "},
        );
    }

    /// a pack unpacked by `**kwargs` under a *different* name leaves the `*args` unpack alone
    #[test]
    fn a_different_pack_leaves_the_vararg_alone() {
        let output = transpile(
            "def f[*Ts, **Kwargs](*args: *Ts, **kwargs: **Kwargs) -> None: ...\n",
            &Config::test_default(),
        )
        .expect("transpile failed");
        assert!(
            output.contains("*args: Unpack[_Ts], **kwargs: _Kwargs.kwargs"),
            "unexpected output:\n{output}"
        );
    }

    /// no python version accepts `*args: *P`, so the pair lowers on 3.11 too
    #[test]
    fn rewrites_forwarded_paramspec_pair_on_311() {
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        let output = transpile(
            "def f[P: (*: *, **: *)](*args: *P, **kwargs: **P) -> None: ...\n",
            &config,
        )
        .expect("transpile failed");
        assert!(
            output.contains("*args: _P.args, **kwargs: _P.kwargs"),
            "unexpected output:\n{output}"
        );
    }
}
