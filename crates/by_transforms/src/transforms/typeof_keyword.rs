//! AST rewrite for the `typeof X` keyword.
//!
//! The parser models `typeof X` as `ExprSubscript { is_typeof: true,
//! value: Name("typeof"), slice: X }`. The AST pass rewrites it to
//! `ExprSubscript { is_typeof: false, value: Name("TypeOf"), slice: X }`
//! so the [`Generator`](`ruff_python_codegen::Generator`) emits
//! `TypeOf[X]`. Nested `typeof` operands rewrite first (post-order),
//! so `typeof typeof X` lowers to `TypeOf[TypeOf[X]]`.
//!
//! A `typeof` nested under a *structural* type-form (`&` / `|` / `and` / `or`
//! / `not` / `?` / a callable arrow) is skipped: the unified type-expression
//! lowerer (`callable`) rewrites the whole expression — `typeof x & int` →
//! `Intersection[TypeOf[x], int]` — via a sub-statement text edit, and this
//! pass firing there would mark the statement changed, re-render it from the
//! mutated AST, and drop that edit (leaving the `&` raw in the output). the
//! skip set is precomputed from the unmutated parse by
//! [`collect_structural_typeof_ranges`]

use std::cell::Cell;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{Transformer, walk_expr};
use ruff_python_ast::visitor::{Visitor, walk_expr as walk_expr_ref};
use ruff_python_ast::{Expr, ExprContext, ExprName, Operator, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping};
use crate::type_info::TypeInfo;

pub(crate) struct TypeofFold {
    changed: Cell<bool>,
    ever_changed: Cell<bool>,
    /// ranges of `typeof` nodes owned by the type-expression lowerer — skipped
    skip: Vec<TextRange>,
}

impl TypeofFold {
    pub(crate) fn new(skip: Vec<TextRange>) -> Self {
        Self {
            changed: Cell::new(false),
            ever_changed: Cell::new(false),
            skip,
        }
    }

    pub(crate) fn changed_cell(&self) -> &Cell<bool> {
        &self.changed
    }

    pub(crate) fn ever_changed(&self) -> bool {
        self.ever_changed.get()
    }
}

impl Transformer for TypeofFold {
    fn visit_expr(&self, expr: &mut Expr) {
        // post-order: nested `typeof` rewrites first
        walk_expr(self, expr);

        if let Expr::Subscript(s) = expr
            && s.is_typeof
            && !self.skip.contains(&s.range())
        {
            s.is_typeof = false;
            *s.value = Expr::Name(ExprName {
                node_index: ruff_python_ast::AtomicNodeIndex::NONE,
                range: TextRange::default(),
                id: Name::from("TypeOf"),
                ctx: ExprContext::Load,
            });
            self.changed.set(true);
            self.ever_changed.set(true);
        }
    }
}

/// a type-form whose lowering the unified type-expression lowerer owns — a
/// `typeof` anywhere inside one must be left to it
fn is_structural_type_form(expr: &Expr) -> bool {
    match expr {
        Expr::BinOp(b) => matches!(b.op, Operator::BitAnd | Operator::BitOr),
        Expr::BoolOp(_) | Expr::CallableType(_) => true,
        Expr::UnaryOp(u) => matches!(u.op, UnaryOp::Not | UnaryOp::Optional),
        _ => false,
    }
}

/// collect the ranges of every `typeof` node nested under a structural
/// type-form in a type position. walked on the unmutated parse, so the ranges
/// match the nodes [`TypeofFold`] later sees
pub(crate) fn collect_structural_typeof_ranges(
    stmts: &[Stmt],
    types: &dyn TypeInfo,
    claimed: &[TextRange],
) -> Vec<TextRange> {
    struct TypeofCollector<'a>(&'a mut Vec<TextRange>);
    impl Visitor<'_> for TypeofCollector<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Subscript(s) = expr
                && s.is_typeof
            {
                self.0.push(s.range());
            }
            walk_expr_ref(self, expr);
        }
    }

    struct Scan(Vec<TextRange>);
    impl TypeExprVisitor for Scan {
        fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
            if is_structural_type_form(expr) {
                TypeofCollector(&mut self.0).visit_expr(expr);
                return Recurse::Stop;
            }
            Recurse::Descend
        }
    }

    let mut scan = Scan(Vec::new());
    walk_type_positions_skipping(stmts, Some(types), claimed, &mut scan);
    scan.0
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
    fn simple() {
        check(
            indoc! {"
                b: int = 1
                a: typeof b = 1
            "},
            indoc! {"
                from ty_extensions import TypeOf
                b: int = 1
                a: TypeOf[b] = 1
            "},
        );
    }

    #[test]
    fn typeof_attribute() {
        check(
            indoc! {"
                a: typeof obj.field = 1
            "},
            indoc! {"
                from ty_extensions import TypeOf
                a: TypeOf[obj.field] = 1
            "},
        );
    }

    #[test]
    fn typeof_in_union() {
        check(
            indoc! {"
                a: typeof b | int = 1
            "},
            indoc! {"
                from ty_extensions import TypeOf
                a: TypeOf[b] | int = 1
            "},
        );
    }

    #[test]
    fn typeof_in_function_signature() {
        check(
            indoc! {"
                def f(x: typeof y) -> typeof z: ...
            "},
            indoc! {"
                from ty_extensions import TypeOf
                def f(x: TypeOf[y]) -> TypeOf[z]:
                    ...
            "},
        );
    }

    #[test]
    fn typeof_identifier_is_passthrough_in_python() {
        unchanged("typeof = 5\n");
    }

    // a `typeof` under a structural type-form is owned by the unified
    // type-expression lowerer — the fold must not fire there, or its
    // statement re-render drops the lowerer's wide edit and the surface
    // operator leaks into the output raw
    #[test]
    fn typeof_in_intersection() {
        check(
            indoc! {"
                b: int = 1
                a: typeof b & int
            "},
            indoc! {"
                from ty_extensions import Intersection, TypeOf
                b: int = 1
                a: Intersection[TypeOf[b], int]
            "},
        );
    }

    #[test]
    fn typeof_in_and_keyword() {
        check(
            indoc! {"
                b: int = 1
                a: typeof b and int
            "},
            indoc! {"
                from ty_extensions import Intersection, TypeOf
                b: int = 1
                a: Intersection[TypeOf[b], int]
            "},
        );
    }

    #[test]
    fn typeof_in_callable_arrow() {
        check(
            indoc! {"
                b: int = 1
                f: (typeof b) -> int
            "},
            indoc! {"
                from ty_extensions import TypeOf
                from typing import Callable
                b: int = 1
                f: Callable[[TypeOf[b]], int]
            "},
        );
    }

    #[test]
    fn typeof_under_not() {
        check(
            indoc! {"
                b: int = 1
                a: not typeof b
            "},
            indoc! {"
                from ty_extensions import Not, TypeOf
                b: int = 1
                a: Not[TypeOf[b]]
            "},
        );
    }

    #[test]
    fn typeof_optional() {
        // the parens are the user's own — the narrow edits keep them
        check(
            indoc! {"
                b: int = 1
                a: (typeof b)?
            "},
            indoc! {"
                from ty_extensions import TypeOf
                b: int = 1
                a: (TypeOf[b]) | None
            "},
        );
    }
}
