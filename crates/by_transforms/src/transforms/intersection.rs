//! Type-aware pass that rewrites intersection types in type positions.
//!
//! `a: A & B`            → `a: Intersection[A, B]`
//! `a: A & B & C`        → `a: Intersection[A, B, C]`
//! `a: (A & B) | C`      → `a: Intersection[A, B] | C`
//! `a: list[A & B]`      → `a: list[Intersection[A, B]]`
//!
//! also lowers the keyword spellings `and` / `or` (`BoolOp`) in type positions —
//! `and` is intersection, `or` is union:
//!
//! `a: A and B`          → `a: Intersection[A, B]`
//! `a: A or B`           → `a: A | B`
//! `a: A and B or C`     → `a: Intersection[A, B] | C`
//!
//! Uses `Intersection` from `ty_extensions`. Fires in every type position
//! recognised by [`type_expr_walker`] — annotations, return types,
//! type-alias RHS, type-param bounds/defaults, class bases, value-position
//! type applications, `cast(T, _)`, `Annotated[T, …]` first arg,
//! `Callable[[P], R]` parameter list + return. Bitwise-AND and boolean
//! `and` / `or` in non-type contexts are never affected.

use ruff_python_ast::{
    AtomicNodeIndex, BoolOp, Expr, ExprBinOp, ExprContext, ExprName, ExprSubscript, ExprTuple,
    Operator, Stmt, UnaryOp, name::Name,
};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass, render_expr};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping};
use crate::type_info::TypeInfo;

pub(crate) struct IntersectionType;

impl IntersectionType {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for IntersectionType {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut state = State {
            edits: Vec::new(),
            needs: LowerImports::default(),
        };
        walk_type_positions_skipping(stmts, Some(types), &ctx.claimed_type_op_ranges, &mut state);
        ctx.text_edits.extend(state.edits);
        state.needs.push_required(ctx);
    }
}

/// which `ty_extensions` imports a [`lower`] run produced. shared with the
/// `not_type` pass, whose operands lower through the same function
#[derive(Default)]
pub(super) struct LowerImports {
    pub(super) intersection: bool,
    pub(super) not: bool,
}

impl LowerImports {
    pub(super) fn push_required(&self, ctx: &mut PassContext) {
        if self.intersection {
            ctx.required_imports
                .push("from ty_extensions import Intersection".to_owned());
        }
        if self.not {
            ctx.required_imports
                .push("from ty_extensions import Not".to_owned());
        }
    }
}

struct State {
    edits: Vec<(TextRange, String)>,
    needs: LowerImports,
}

impl TypeExprVisitor for State {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // collapse the whole `&` / `and` / `or` chain into one edit so no stray
        // surface operator survives between per-arm rewrites
        if is_intersection_node(expr) || matches!(expr, Expr::BoolOp(_)) {
            let new_node = lower(expr, &mut self.needs);
            self.edits.push((expr.range(), render_expr(&new_node)));
            return Recurse::Stop;
        }
        Recurse::Descend
    }
}

/// `&` and its keyword spelling `and` both denote an intersection
fn is_intersection_node(expr: &Expr) -> bool {
    match expr {
        Expr::BinOp(b) => matches!(b.op, Operator::BitAnd),
        Expr::BoolOp(b) => matches!(b.op, BoolOp::And),
        _ => false,
    }
}

/// build `Intersection[…]` from the flattened arms of an intersection chain
/// (collected by [`collect_intersect`], which always yields ≥ 2 operands).
/// each arm may itself contain a nested intersection or union inside a
/// subscript — recursively lower them so the rendered output is fully
/// rewritten in one shot
fn build_intersection(operands: &[Expr], needs: &mut LowerImports) -> Expr {
    needs.intersection = true;
    let mut elts: Vec<Expr> = operands.iter().map(|e| lower(e, needs)).collect();
    let slice = if elts.len() == 1 {
        elts.remove(0)
    } else {
        Expr::Tuple(ExprTuple {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            elts,
            ctx: ExprContext::Load,
            parenthesized: false,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            parameter_slash: None,
            parameter_star: None,
            is_parameter_shape: false,
        })
    };
    subscript("Intersection", slice)
}

fn subscript(name: &str, slice: Expr) -> Expr {
    Expr::Subscript(ExprSubscript {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        value: Box::new(Expr::Name(ExprName {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            id: Name::from(name),
            ctx: ExprContext::Load,
        })),
        slice: Box::new(slice),
        ctx: ExprContext::Load,
        is_typeof: false,
    })
}

/// recursively lower intersection (`&` / `and`), keyword-union (`or`), and
/// negation (`not`) chains nested inside an arm of an outer rewrite. used to
/// build a single rendered output for the wide edit. other subtrees are
/// returned unchanged (cloned). records in `needs` whichever `ty_extensions`
/// names the result references
pub(super) fn lower(expr: &Expr, needs: &mut LowerImports) -> Expr {
    match expr {
        _ if is_intersection_node(expr) => {
            let mut operands: Vec<Expr> = Vec::new();
            collect_intersect(expr, &mut operands);
            build_intersection(&operands, needs)
        }
        Expr::BoolOp(b) if matches!(b.op, BoolOp::Or) => {
            let mut operands: Vec<Expr> = Vec::new();
            collect_union(expr, &mut operands);
            let arms = operands.iter().map(|v| lower(v, needs));
            union_of(arms).unwrap_or_else(|| expr.clone())
        }
        Expr::UnaryOp(u) if matches!(u.op, UnaryOp::Not) => {
            needs.not = true;
            subscript("Not", lower(&u.operand, needs))
        }
        Expr::BinOp(b) if matches!(b.op, Operator::BitOr) => {
            let mut new_b = b.clone();
            *new_b.left = lower(&b.left, needs);
            *new_b.right = lower(&b.right, needs);
            Expr::BinOp(new_b)
        }
        Expr::Subscript(s) => {
            // mirror the walker's special forms: `Literal[…]` args are value
            // tokens, `Annotated[T, …]` is a type only in its first slot
            if name_is(&s.value, "Literal") {
                return expr.clone();
            }
            let mut new_s = s.clone();
            let new_slice = match s.slice.as_ref() {
                Expr::Tuple(t) if !t.parenthesized => {
                    let mut nt = t.clone();
                    if name_is(&s.value, "Annotated") {
                        if let Some(first) = nt.elts.first_mut() {
                            *first = lower(first, needs);
                        }
                    } else {
                        nt.elts = t.elts.iter().map(|e| lower(e, needs)).collect();
                    }
                    Expr::Tuple(nt)
                }
                other => lower(other, needs),
            };
            *new_s.slice = new_slice;
            Expr::Subscript(new_s)
        }
        _ => expr.clone(),
    }
}

fn name_is(expr: &Expr, ident: &str) -> bool {
    match expr {
        Expr::Name(n) => n.id.as_str() == ident,
        Expr::Attribute(a) => a.attr.id.as_str() == ident,
        _ => false,
    }
}

/// fold lowered union arms into a left-associative `|` chain
fn union_of(arms: impl Iterator<Item = Expr>) -> Option<Expr> {
    arms.reduce(|left, right| {
        Expr::BinOp(ExprBinOp {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            left: Box::new(left),
            op: Operator::BitOr,
            right: Box::new(right),
        })
    })
}

/// flatten a keyword-union chain — `|` `BinOp`s and `or` `BoolOp`s mix freely
/// (`A or B | C` folds into one left-associative `|` chain so the rendered
/// output carries no redundant parentheses)
fn collect_union(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BoolOp(b) if matches!(b.op, BoolOp::Or) => {
            for v in &b.values {
                collect_union(v, out);
            }
        }
        Expr::BinOp(b) if matches!(b.op, Operator::BitOr) => {
            collect_union(&b.left, out);
            collect_union(&b.right, out);
        }
        _ => out.push(expr.clone()),
    }
}

/// flatten an intersection chain — `&` `BinOp`s and `and` `BoolOp`s mix freely
/// (`A & B and C` is one three-arm intersection)
fn collect_intersect(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
            collect_intersect(&b.left, out);
            collect_intersect(&b.right, out);
        }
        Expr::BoolOp(b) if matches!(b.op, BoolOp::And) => {
            for v in &b.values {
                collect_intersect(v, out);
            }
        }
        _ => out.push(expr.clone()),
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
    fn simple_two_type() {
        check(
            "a: A & B\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B]
            "},
        );
    }

    #[test]
    fn three_types() {
        check(
            "a: A & B & C\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B, C]
            "},
        );
    }

    #[test]
    fn intersection_with_union() {
        check(
            "a: (A & B) | C\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B] | C
            "},
        );
    }

    #[test]
    fn nested_inside_list() {
        check(
            "a: list[A & B]\n",
            indoc! {"
                from ty_extensions import Intersection
                a: list[Intersection[A, B]]
            "},
        );
    }

    #[test]
    fn function_parameter() {
        check(
            indoc! {"
                def f(x: A & B) -> A & C:
                    pass
            "},
            indoc! {"
                from ty_extensions import Intersection
                def f(x: Intersection[A, B]) -> Intersection[A, C]:
                    pass
            "},
        );
    }

    #[test]
    fn value_context_unchanged() {
        check("x = A & B\n", "x = A & B\n");
    }

    #[test]
    fn augmented_assign_unchanged() {
        check("x &= B\n", "x &= B\n");
    }

    #[test]
    fn python_unchanged() {
        unchanged("a: A & B\n");
    }

    #[test]
    fn intersection_in_union_arm() {
        // BinOp `|` must descend into both arms — `int | (A & B)` had been
        // missed by the old direct-recursion walker
        check(
            "a: int | (A & B)\n",
            indoc! {"
                from ty_extensions import Intersection
                a: int | Intersection[A, B]
            "},
        );
    }

    #[test]
    fn nested_intersection_in_dict_value() {
        check(
            "a: dict[str, A & B]\n",
            indoc! {"
                from ty_extensions import Intersection
                a: dict[str, Intersection[A, B]]
            "},
        );
    }

    #[test]
    fn intersection_in_type_alias_rhs() {
        check_py312(
            "type X = A & B\n",
            indoc! {"
                from ty_extensions import Intersection
                type X = Intersection[A, B]
            "},
        );
    }

    #[test]
    fn intersection_in_typeparam_bound() {
        check_py312(
            "def f[T: A & B](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Intersection
                def f[T: Intersection[A, B]](x: T) -> T: ...
            "},
        );
    }

    #[test]
    fn intersection_in_typeparam_default() {
        check_py312(
            "def f[T = A & B](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Intersection
                def f[T = Intersection[A, B]](x: T) -> T: ...
            "},
        );
    }

    #[test]
    fn intersection_in_class_base() {
        check(
            "class C(list[A & B]): ...\n",
            indoc! {"
                from ty_extensions import Intersection
                class C(list[Intersection[A, B]]): ...
            "},
        );
    }

    #[test]
    fn intersection_in_value_position_type_application() {
        check(
            "reveal_type(list[A & B])\n",
            indoc! {"
                from ty_extensions import Intersection
                reveal_type(list[Intersection[A, B]])
            "},
        );
    }

    #[test]
    fn intersection_in_cast_first_arg() {
        check(
            "from typing import cast\nb = cast(A & B, a)\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import cast
                b = cast(Intersection[A, B], a)
            "},
        );
    }

    #[test]
    fn intersection_in_callable_param_and_return() {
        check(
            "from typing import Callable\nf: Callable[[A & B], C & D]\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import Callable
                f: Callable[[Intersection[A, B]], Intersection[C, D]]
            "},
        );
    }

    #[test]
    fn intersection_in_annotated_first_arg_only() {
        // metadata in `Annotated[T, …]` must remain untouched
        check(
            "from typing import Annotated\na: Annotated[A & B, \"doc\"]\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import Annotated
                a: Annotated[Intersection[A, B], \"doc\"]
            "},
        );
    }

    #[test]
    fn intersection_inside_literal_opaque() {
        // `Literal[...]` slice contents are value tokens, not type
        // expressions — bitwise-AND inside Literal is unchanged
        unchanged("from typing import Literal\na: Literal[1, 2]\n");
    }

    #[test]
    fn or_keyword_is_union() {
        check("a: A or B\n", "a: A | B\n");
    }

    #[test]
    fn or_keyword_nary_chain() {
        check("a: A or B or C\n", "a: A | B | C\n");
    }

    #[test]
    fn and_keyword_is_intersection() {
        check(
            "a: A and B\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B]
            "},
        );
    }

    #[test]
    fn and_keyword_nary_chain() {
        check(
            "a: A and B and C\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B, C]
            "},
        );
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // matches python's boolean precedence — and over or, like & over |
        check(
            "a: A and B or C\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B] | C
            "},
        );
    }

    #[test]
    fn parenthesized_or_inside_and() {
        check(
            "a: (A or B) and C\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A | B, C]
            "},
        );
    }

    #[test]
    fn keyword_and_symbol_mix_flattens() {
        // `&` binds tighter than `and`; same operator, one flat intersection
        check(
            "a: A & B and C\n",
            indoc! {"
                from ty_extensions import Intersection
                a: Intersection[A, B, C]
            "},
        );
    }

    #[test]
    fn or_keyword_with_pipe_union() {
        // `|` binds tighter than `or`; both are union, output is one chain
        check("a: A or B | C\n", "a: A | B | C\n");
    }

    #[test]
    fn or_keyword_nested_in_generic() {
        check("a: list[A or B]\n", "a: list[A | B]\n");
    }

    #[test]
    fn and_keyword_nested_in_generic() {
        check(
            "a: dict[str, A and B]\n",
            indoc! {"
                from ty_extensions import Intersection
                a: dict[str, Intersection[A, B]]
            "},
        );
    }

    #[test]
    fn keyword_ops_in_function_signature() {
        check(
            indoc! {"
                def f(x: A or B) -> A and C:
                    pass
            "},
            indoc! {"
                from ty_extensions import Intersection
                def f(x: A | B) -> Intersection[A, C]:
                    pass
            "},
        );
    }

    #[test]
    fn or_keyword_arm_with_subscript() {
        check(
            "a: list[A and B] or None\n",
            indoc! {"
                from ty_extensions import Intersection
                a: list[Intersection[A, B]] | None
            "},
        );
    }

    #[test]
    fn and_keyword_with_not_arm() {
        check(
            "a: A and not B\n",
            indoc! {"
                from ty_extensions import Intersection, Not
                a: Intersection[A, Not[B]]
            "},
        );
    }

    #[test]
    fn keyword_ops_in_type_alias_rhs() {
        check_py312(
            "type X = A and B or C\n",
            indoc! {"
                from ty_extensions import Intersection
                type X = Intersection[A, B] | C
            "},
        );
    }

    #[test]
    fn keyword_ops_in_typeparam_bound() {
        check_py312(
            "def f[T: A or B](x: T) -> T: ...\n",
            "def f[T: A | B](x: T) -> T: ...\n",
        );
    }

    #[test]
    fn keyword_ops_in_cast_first_arg() {
        check(
            "from typing import cast\nb = cast(A and B, a)\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import cast
                b = cast(Intersection[A, B], a)
            "},
        );
    }

    #[test]
    fn keyword_ops_in_annotated_first_arg_only() {
        check(
            "from typing import Annotated\na: Annotated[A or B, \"doc\"]\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[A | B, \"doc\"]
            "},
        );
    }

    #[test]
    fn value_position_boolop_unchanged() {
        check("x = A or B\n", "x = A or B\n");
    }

    #[test]
    fn value_position_and_unchanged() {
        check("x = A and B\n", "x = A and B\n");
    }

    #[test]
    fn condition_boolop_unchanged() {
        check(
            indoc! {"
                if a and b or c:
                    pass
            "},
            indoc! {"
                if a and b or c:
                    pass
            "},
        );
    }

    #[test]
    fn python_or_keyword_unchanged() {
        unchanged("a: A or B\n");
    }

    #[test]
    fn python_and_keyword_unchanged() {
        unchanged("a: A and B\n");
    }
}
