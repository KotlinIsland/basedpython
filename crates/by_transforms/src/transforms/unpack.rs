//! AST pass: polyfills PEP 646 starred-type syntax in variadic parameter
//! annotations and inside subscript slices.
//!
//! `def f(*args: *tuple[int, ...])` → `def f(*args: Unpack[tuple[int, ...]])`
//! `tuple[*Ts]`                     → `tuple[Unpack[Ts]]`
//! `class Stack(Generic[*Ts]):`     → `class Stack(Generic[Unpack[Ts]]):`
//!
//! A starred subscript that is not a type is the tuple python 3.11 reads it as:
//!
//! `d[*a]`                          → `d[(*a,)]`
//!
//! Also lowers basedpython's pack forwarding, which no python version accepts:
//!
//! `def f(**kwargs: **Kwargs)`      → `def f(**kwargs: Kwargs.kwargs)`
//! `def f(*args: *P, **kwargs: **P)` → `def f(*args: P.args, **kwargs: P.kwargs)`

use std::cell::RefCell;
use std::collections::HashSet;

use ruff_python_ast::PythonVersion;
use ruff_python_ast::helpers::top_star_slice_elements;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};
use crate::config::Config;
use crate::type_info::TypeInfo;

pub(crate) struct UnpackSyntax {
    config: Config,
    type_subscripts: TypeSubscripts,
}

impl UnpackSyntax {
    pub(crate) fn new(config: Config, type_subscripts: TypeSubscripts) -> Self {
        Self {
            config,
            type_subscripts,
        }
    }
}

/// the ranges of the subscripts that are type expressions — the ones whose starred elements
/// are spelled `Unpack[...]` rather than as the tuple python reads a starred subscript as
#[derive(Default)]
pub(crate) struct TypeSubscripts(HashSet<TextRange>);

/// which subscripts in `stmts` are type expressions, as the type checker reads them
pub(crate) fn collect_type_subscripts(stmts: &[Stmt], types: &dyn TypeInfo) -> TypeSubscripts {
    let mut subscripts = TypeSubscripts::default();
    walk_type_positions(stmts, Some(types), &mut subscripts);
    subscripts
}

impl TypeExprVisitor for TypeSubscripts {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::Subscript(subscript) = expr {
            self.0.insert(subscript.range());
        }
        Recurse::Descend
    }
}

impl AstPass for UnpackSyntax {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::Unpack)
    }

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
            type_subscripts: &self.type_subscripts,
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

/// The `*` of an unpack, which is what [`Unpack`] replaces.
///
/// Only the star itself: the type after it can be parenthesized, `*((int, str) * n)`, and a
/// range reaching to the start of that type would take the opening parenthesis with it, and
/// collide with the edit that lowers the type inside it.
fn star_token_range(unpack: TextRange) -> TextRange {
    TextRange::at(unpack.start(), TextSize::from(1))
}

struct State<'a> {
    edits: RefCell<Vec<(TextRange, String)>>,
    needs_import: bool,
    lowered_varargs: Vec<TextRange>,
    type_subscripts: &'a TypeSubscripts,
}

impl State<'_> {
    /// `d[*a]` → `d[(*a,)]` and `d[*a, b]` → `d[(*a, b)]`: the tuple python 3.11 builds
    /// for a starred subscript, which earlier versions only accept parenthesized
    fn parenthesize_value_slice(&self, slice: &ruff_python_ast::ExprTuple) {
        let mut edits = self.edits.borrow_mut();
        edits.push((TextRange::empty(slice.start()), "(".to_owned()));
        // a tuple of one element needs a comma, which its range takes in when written
        let needs_comma = matches!(&*slice.elts, [only] if only.end() == slice.end());
        let closing = if needs_comma { ",)" } else { ")" };
        edits.push((TextRange::empty(slice.end()), closing.to_owned()));
    }

    fn rewrite_subscript_starred(&mut self, starred: &ruff_python_ast::ExprStarred) {
        self.needs_import = true;
        let star_range = star_token_range(starred.range());
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
        let star_range = star_token_range(ann.range());
        self.edits
            .borrow_mut()
            .push((star_range, "Unpack[".to_owned()));
        let end = ann.range().end();
        self.edits
            .borrow_mut()
            .push((TextRange::new(end, end), "]".to_owned()));
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
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
            let is_type = self.type_subscripts.0.contains(&s.range());
            match s.slice.as_ref() {
                Expr::Tuple(t)
                    if !is_type
                        && !t.parenthesized
                        && !t.has_parameter_shape()
                        && t.elts.iter().any(Expr::is_starred_expr) =>
                {
                    self.parenthesize_value_slice(t);
                }
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

    fn check_py311(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
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

    /// a tuple type inside an unpacked annotation is lowered like any other type position. the
    /// `Unpack[` replacement covers the star alone, so it does not collide with the lowering of a
    /// parenthesized type after it
    #[test]
    fn rewrites_tuple_type_inside_starred_vararg_annotation() {
        check(
            "def f(*args: *(int, str)): ...\ndef g(*args: *((int, str) * int)): ...\n",
            indoc! {"
                from typing_extensions import Unpack
                def f(*args: Unpack[tuple[int, str]]): ...
                def g(*args: Unpack[(tuple[int | str, ...])]): ...
            "},
        );
    }

    #[test]
    fn starred_vararg_annotation_keeps_the_star_on_py311() {
        check_py311(
            "def f(*args: *((int, str) * 2)): ...\n",
            "def f(*args: *(tuple[int, str, int, str])): ...\n",
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

    /// a starred subscript of a value is the tuple of what it unpacks, which python before
    /// 3.11 spells with parentheses. `Unpack[a]` there would look `a` up as a type
    #[test]
    fn a_value_subscript_is_the_tuple_python_reads() {
        check(
            indoc! {"
                d = {(1, 2): \"x\", (1, 2, 3): \"y\"}
                a = (1, 2)
                print(d[*a], d[*a,], d[*a, 3], d[*a, 3,])
            "},
            indoc! {"
                d = {(1, 2): \"x\", (1, 2, 3): \"y\"}
                a = (1, 2)
                print(d[(*a,)], d[(*a,)], d[(*a, 3)], d[(*a, 3,)])
            "},
        );
    }

    /// whether a subscript is a type is the type checker's answer, so a generic type
    /// applied outside an annotation is still an unpack, and a value in the metadata of
    /// an annotation is still a tuple
    #[test]
    fn a_type_subscript_outside_an_annotation_is_an_unpack() {
        check(
            indoc! {"
                from typing import Annotated, TypeVarTuple
                Ts = TypeVarTuple(\"Ts\")
                d = {(1,): 1}
                a = (1,)
                Alias = tuple[int, *Ts]
                x: Annotated[tuple[*Ts], d[*a]]
            "},
            indoc! {"
                from typing import Annotated
                from typing_extensions import TypeVarTuple, Unpack
                Ts = TypeVarTuple(\"Ts\")
                d = {(1,): 1}
                a = (1,)
                Alias = tuple[int, Unpack[Ts]]
                x: Annotated[tuple[Unpack[Ts]], d[(*a,)]]
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
