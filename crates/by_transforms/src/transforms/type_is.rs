//! AST pass that rewrites `a is T` narrowing-predicate syntax in type
//! positions to `typing.TypeIs[T]`.
//!
//! `def f(a) -> a is int: ...` → `def f(a) -> TypeIs[int]: ...`
//!
//! basedpython surface syntax for narrowing predicates names the parameter
//! being narrowed alongside its target type. The runtime semantics are
//! identical to PEP 742 `TypeIs[T]`; the parameter name is lost in
//! lowering since `TypeIs` doesn't carry it.
//!
//! traversal is delegated to [`type_expr_walker`] (with `types = None` —
//! value-position `a is T` is *not* a type expression here; it's the
//! basedpython surface form for `isinstance(a, T)`, owned by
//! `identity_swap`). running before `identity_swap` in the `AstPass` list so
//! type-position rewrites win the first-wins overlap dedup

use ruff_python_ast::helpers::{ReturnGuardForm, return_guards};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{
    AtomicNodeIndex, CmpOp, Expr, ExprContext, ExprName, ExprSubscript, ModModule, Stmt,
    StmtFunctionDef, name::Name,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext, render_expr};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping};

pub(crate) struct TypeIs<'src> {
    src: &'src str,
}

impl<'src> TypeIs<'src> {
    pub(crate) fn new(src: &'src str) -> Self {
        Self { src }
    }
}

impl AstPass for TypeIs<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            edits: Vec::new(),
            needs_import: false,
        };
        let body: &[Stmt] = &module.body;

        // narrowing annotations that name a place python can't: an assertion guard, and a
        // predicate on something other than a parameter. both lower to what the function
        // returns, and their ranges are claimed so the `TypeIs[T]` rewrite below skips them
        let mut guards = ReturnGuards {
            src: self.src,
            edits: Vec::new(),
            claimed: Vec::new(),
        };
        for stmt in body {
            guards.visit_stmt(stmt);
        }
        state.edits.append(&mut guards.edits);

        walk_type_positions_skipping(body, None, &guards.claimed, &mut state);
        ctx.text_edits.extend(state.edits);
        if state.needs_import {
            // typing.TypeIs landed in 3.13 (PEP 742). on older runtimes the
            // typing_redirect pass switches the import to typing_extensions
            ctx.required_imports
                .push("from typing import TypeIs".to_owned());
        }
    }
}

struct State {
    edits: Vec<(TextRange, String)>,
    needs_import: bool,
}

/// lowers the narrowing return annotations that have no `TypeIs` spelling
struct ReturnGuards<'src> {
    src: &'src str,
    edits: Vec<(TextRange, String)>,
    claimed: Vec<TextRange>,
}

impl ReturnGuards<'_> {
    fn function(&mut self, function: &StmtFunctionDef) {
        let Some(returns) = function.returns.as_deref() else {
            return;
        };

        // `def f(x) -> asserts x` raises when the assertion doesn't hold, and returns
        // `None` when it does. the keyword is not part of `returns`, so the edit starts
        // at the keyword itself
        if function.is_asserts_return {
            let keyword_start = self.src[..usize::from(returns.range().start())]
                .rfind("asserts")
                .map(|offset| TextSize::try_from(offset).expect("offset fits u32"))
                .unwrap_or_else(|| returns.range().start());
            self.edits.push((
                TextRange::new(keyword_start, returns.range().end()),
                "None".to_owned(),
            ));
            self.claimed.push(returns.range());
            return;
        }

        // `def f() -> a is int` narrows a place rather than an argument, and
        // `-> self.data is str` narrows a member of one. `TypeIs` can only name a bare
        // parameter, so anything else lowers to the `bool` the function returns
        if let Some(guards) = return_guards(function)
            && let [guard] = guards.as_slice()
            && matches!(guard.form, ReturnGuardForm::Predicate { .. })
        {
            let (name, members) = guard.place_parts();
            let narrows_a_parameter = members.is_empty()
                && function
                    .parameters
                    .iter()
                    .any(|parameter| parameter.name().id == *name);
            if !narrows_a_parameter {
                self.edits.push((returns.range(), "bool".to_owned()));
                self.claimed.push(returns.range());
            }
        }
    }
}

impl<'ast> Visitor<'ast> for ReturnGuards<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            self.function(function);
        }
        walk_stmt(self, stmt);
    }
}

impl TypeExprVisitor for State {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::Compare(c) = expr
            && c.ops.len() == 1
            && matches!(c.ops[0], CmpOp::Is)
            && matches!(c.left.as_ref(), Expr::Name(_))
            && let Some(target) = c.comparators.first()
        {
            let new_node = Expr::Subscript(ExprSubscript {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                value: Box::new(Expr::Name(ExprName {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                    id: Name::from("TypeIs"),
                    ctx: ExprContext::Load,
                })),
                slice: Box::new(target.clone()),
                ctx: ExprContext::Load,
                is_typeof: false,
            });
            self.needs_import = true;
            self.edits.push((expr.range(), render_expr(&new_node)));
        }
        Recurse::Descend
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn check_py312(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple() {
        check(
            "def f(a) -> a is int: ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def f(a) -> TypeIs[int]: ...
            "},
        );
    }

    #[test]
    fn other_param_name() {
        check(
            "def is_str(x) -> x is str: ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def is_str(x) -> TypeIs[str]: ...
            "},
        );
    }

    #[test]
    fn asserts_returns_none() {
        check(
            "def check(x: int | None) -> asserts x:\n    if x is None:\n        raise ValueError\n",
            indoc! {"
                def check(x: int | None) -> None:
                    if x is None:
                        raise ValueError
            "},
        );
    }

    #[test]
    fn negated_asserts_returns_none() {
        check(
            "def check(x: int | None) -> asserts not x: ...\n",
            indoc! {"
                def check(x: int | None) -> None: ...
            "},
        );
    }

    #[test]
    fn typed_asserts_returns_none() {
        check(
            "def check(x: int | None) -> asserts x is int: ...\n",
            indoc! {"
                def check(x: int | None) -> None: ...
            "},
        );
        check(
            "def check(x: int | None) -> asserts x is not None: ...\n",
            indoc! {"
                def check(x: int | None) -> None: ...
            "},
        );
    }

    #[test]
    fn member_guards_lower_to_what_they_return() {
        // `TypeIs` can only name a bare parameter, so a member predicate lowers to `bool`
        check(
            "class C:\n    data: str | None = None\n    def ensure(self) -> asserts self.data is not None: ...\n    def loaded(self) -> self.data is str: ...\n",
            indoc! {"
                class C:
                    data: str | None = None
                    def ensure(self) -> None: ...
                    def loaded(self) -> bool: ...
            "},
        );
    }

    #[test]
    fn several_asserted_places_return_none() {
        check(
            "def check(a: int | None, b: str | None) -> asserts a is int and b: ...\n",
            indoc! {"
                def check(a: int | None, b: str | None) -> None: ...
            "},
        );
    }

    #[test]
    fn asserts_method_returns_none() {
        check(
            "class C:\n    def check(self, y: int | None) -> asserts y: ...\n",
            indoc! {"
                class C:
                    def check(self, y: int | None) -> None: ...
            "},
        );
    }

    #[test]
    fn non_parameter_predicate_returns_bool() {
        // `TypeIs` can only name a parameter, so a predicate on a place lowers to `bool`
        check(
            "a = 1\ndef f() -> a is int:\n    return True\n",
            indoc! {"
                a = 1
                def f() -> bool:
                    return True
            "},
        );
    }

    #[test]
    fn body_is_value_unchanged() {
        unchanged("def f(a):\n    return a is None\n");
    }

    #[test]
    fn predicate_in_param_annotation() {
        // walker now exposes the predicate syntax in any type position. param
        // annotations are unusual but consistent — `x: a is int` lowers
        check(
            "def f(x: a is int): ...\n",
            indoc! {"
                from typing_extensions import TypeIs
                def f(x: TypeIs[int]): ...
            "},
        );
    }

    #[test]
    fn predicate_in_ann_assign() {
        check(
            "b: a is int\n",
            indoc! {"
                from typing_extensions import TypeIs
                b: TypeIs[int]
            "},
        );
    }

    #[test]
    fn predicate_in_type_alias_rhs() {
        check_py312(
            "type Pred = a is int\n",
            indoc! {"
                from typing_extensions import TypeIs
                type Pred = TypeIs[int]
            "},
        );
    }
}
