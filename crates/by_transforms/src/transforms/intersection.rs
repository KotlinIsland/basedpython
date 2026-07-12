//! Shared helpers for intersection / union chains in type positions, plus the
//! end-to-end tests for the surface syntax.
//!
//! The lowering itself (`A & B` → `Intersection[A, B]`, `A and B` likewise,
//! `A or B` → `A | B`) lives in the single type-expression lowerer in
//! [`callable`](super::callable), which owns every structural type-form so there
//! is one implementation. This module exposes the chain-flattening helpers that
//! lowerer reuses: `&` `BinOp`s and `and` `BoolOp`s flatten into one
//! intersection (`A & B and C` is a single `Intersection[A, B, C]`), and `|` and
//! `or` flatten into one union.

use ruff_python_ast::{BoolOp, Expr, Operator};

/// `&` and its keyword spelling `and` both denote an intersection
pub(crate) fn is_intersection_node(expr: &Expr) -> bool {
    match expr {
        Expr::BinOp(b) => matches!(b.op, Operator::BitAnd),
        Expr::BoolOp(b) => matches!(b.op, BoolOp::And),
        _ => false,
    }
}

/// flatten a keyword-union chain — `|` `BinOp`s and `or` `BoolOp`s mix freely
/// (`A or B | C` folds into one left-associative `|` chain so the rendered
/// output carries no redundant parentheses)
pub(crate) fn collect_union(expr: &Expr, out: &mut Vec<Expr>) {
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
pub(crate) fn collect_intersect(expr: &Expr, out: &mut Vec<Expr>) {
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
        check_at(input, expected, PythonVersion::PY312);
    }

    fn check_at(input: &str, expected: &str, version: PythonVersion) {
        let config = Config {
            min_version: version,
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
        // native passthrough of a defaulted list needs 3.13 (pep 696)
        check_at(
            "def f[T = A & B](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Intersection
                def f[T = Intersection[A, B]](x: T) -> T: ...
            "},
            PythonVersion::PY313,
        );
    }

    #[test]
    fn intersection_in_typeparam_default_downlevels_on_312() {
        // on a 3.12 target the defaulted list polyfills, and the intersection
        // still lowers inside the `default=` argument
        check_py312(
            "def f[T = A & B](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", default=Intersection[A, B])
                def f(x: _T) -> _T: ...
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

    // a leaf arm that another type-aware pass would rewrite (`float` →
    // `JustFloat`, a bare literal → `Literal[…]`) must still be lowered, and
    // that pass's import must not orphan — the wide intersection edit used to
    // swallow the narrow leaf edit while keeping its import
    #[test]
    fn intersection_arm_float_composes() {
        check(
            "a: A & float\n",
            indoc! {"
                from ty_extensions import Intersection, JustFloat
                a: Intersection[A, JustFloat]
            "},
        );
    }

    #[test]
    fn and_keyword_arm_float_composes() {
        check(
            "a: A and float\n",
            indoc! {"
                from ty_extensions import Intersection, JustFloat
                a: Intersection[A, JustFloat]
            "},
        );
    }

    #[test]
    fn intersection_arm_literal_composes() {
        check(
            "a: A & 1\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import Literal
                a: Intersection[A, Literal[1]]
            "},
        );
    }

    #[test]
    fn union_arm_float_composes() {
        check(
            "a: float or B\n",
            indoc! {"
                from ty_extensions import JustFloat
                a: JustFloat | B
            "},
        );
    }

    #[test]
    fn intersection_arm_float_in_subscript_composes() {
        check(
            "a: A & list[float]\n",
            indoc! {"
                from ty_extensions import Intersection, JustFloat
                a: Intersection[A, list[JustFloat]]
            "},
        );
    }
}
