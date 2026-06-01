//! Type-aware pass that rewrites intersection types in type positions.
//!
//! `a: A & B`            → `a: Intersection[A, B]`
//! `a: A & B & C`        → `a: Intersection[A, B, C]`
//! `a: (A & B) | C`      → `a: Intersection[A, B] | C`
//! `a: list[A & B]`      → `a: list[Intersection[A, B]]`
//!
//! Uses `Intersection` from `ty_extensions`. Fires in every type position
//! recognised by [`type_expr_walker`] — annotations, return types,
//! type-alias RHS, type-param bounds/defaults, class bases, value-position
//! type applications, `cast(T, _)`, `Annotated[T, …]` first arg,
//! `Callable[[P], R]` parameter list + return. Bitwise-AND in non-type
//! contexts is never affected.

use ruff_python_ast::{
    AtomicNodeIndex, Expr, ExprContext, ExprName, ExprSubscript, ExprTuple, Operator, Stmt,
    name::Name,
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
            needs_import: false,
        };
        walk_type_positions_skipping(stmts, Some(types), &ctx.claimed_type_op_ranges, &mut state);
        ctx.text_edits.extend(state.edits);
        if state.needs_import {
            ctx.required_imports
                .push("from ty_extensions import Intersection".to_owned());
        }
    }
}

struct State {
    edits: Vec<(TextRange, String)>,
    needs_import: bool,
}

impl TypeExprVisitor for State {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // collapse the whole `A & B & C` chain into one edit so no stray
        // `&` survives between per-arm rewrites
        if let Expr::BinOp(b) = expr
            && matches!(b.op, Operator::BitAnd)
        {
            let mut operands: Vec<Expr> = Vec::new();
            collect_bitand(expr, &mut operands);
            let (head, rest) = operands
                .split_first()
                .expect("BitAnd flattens to ≥2 operands");
            let new_node = build_intersection(head, rest);
            self.needs_import = true;
            self.edits.push((expr.range(), render_expr(&new_node)));
            return Recurse::Stop;
        }
        Recurse::Descend
    }
}

/// build `Intersection[head, rest..]`. `head` plus `rest` are the flattened
/// arms of a `&` chain (collected by [`collect_bitand`], which always yields
/// ≥ 2 operands). each arm may itself contain a nested intersection inside
/// a subscript — recursively lower them so the rendered output is fully
/// rewritten in one shot
fn build_intersection(head: &Expr, rest: &[Expr]) -> Expr {
    let head_lowered = lower(head);
    let slice = if rest.is_empty() {
        head_lowered
    } else {
        let mut elts = Vec::with_capacity(1 + rest.len());
        elts.push(head_lowered);
        elts.extend(rest.iter().map(lower));
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
    Expr::Subscript(ExprSubscript {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        value: Box::new(Expr::Name(ExprName {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            id: Name::from("Intersection"),
            ctx: ExprContext::Load,
        })),
        slice: Box::new(slice),
        ctx: ExprContext::Load,
        is_typeof: false,
    })
}

/// recursively lower `&` chains nested inside an arm of an outer
/// intersection. used to build a single rendered output for the wide edit.
/// non-`&` subtrees are returned unchanged (cloned)
fn lower(expr: &Expr) -> Expr {
    match expr {
        Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
            let mut operands: Vec<Expr> = Vec::new();
            collect_bitand(expr, &mut operands);
            let (head, rest) = operands
                .split_first()
                .expect("BitAnd flattens to ≥2 operands");
            build_intersection(head, rest)
        }
        Expr::BinOp(b) if matches!(b.op, Operator::BitOr) => {
            let mut new_b = b.clone();
            *new_b.left = lower(&b.left);
            *new_b.right = lower(&b.right);
            Expr::BinOp(new_b)
        }
        Expr::Subscript(s) => {
            let mut new_s = s.clone();
            let new_slice = match s.slice.as_ref() {
                Expr::Tuple(t) if !t.parenthesized => {
                    let mut nt = t.clone();
                    nt.elts = t.elts.iter().map(lower).collect();
                    Expr::Tuple(nt)
                }
                other => lower(other),
            };
            *new_s.slice = new_slice;
            Expr::Subscript(new_s)
        }
        _ => expr.clone(),
    }
}

fn collect_bitand(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::BinOp(b) = expr
        && matches!(b.op, Operator::BitAnd)
    {
        collect_bitand(&b.left, out);
        collect_bitand(&b.right, out);
    } else {
        out.push(expr.clone());
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
}
