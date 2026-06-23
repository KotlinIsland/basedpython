//! Type-aware pass that rewrites `not T` in type positions to
//! `ty_extensions.Not[T]`.
//!
//! `a: not int`           → `a: Not[int]`
//! `a: not (int | str)`   → `a: Not[int | str]`
//! `a: list[not int]`     → `a: list[Not[int]]`
//!
//! Fires in every type position recognised by [`type_expr_walker`]:
//! annotations, return types, type-alias RHS, type-param bounds/defaults,
//! class bases, value-position type applications, `cast(T, _)`,
//! `Annotated[T, …]` first arg, `Callable[[P], R]` parameter list + return.
//! `not x` in non-type contexts (boolean negation) is never affected.

use ruff_python_ast::{Expr, Stmt, UnaryOp};
use ruff_text_size::TextRange;

use super::ast_driver::{PassContext, TypeAwarePass, render_expr};
use super::intersection::{LowerImports, lower};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};
use crate::type_info::TypeInfo;

pub(crate) struct NotType;

impl NotType {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for NotType {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut state = State {
            edits: Vec::new(),
            needs: LowerImports::default(),
        };
        walk_type_positions(stmts, Some(types), &mut state);
        ctx.text_edits.extend(state.edits);
        state.needs.push_required(ctx);
    }
}

struct State {
    edits: Vec<(TextRange, String)>,
    needs: LowerImports,
}

impl TypeExprVisitor for State {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::UnaryOp(u) = expr
            && matches!(u.op, UnaryOp::Not)
        {
            // the shared lowering rewrites the whole `not …` chain, including
            // `&` / `and` / `or` and nested `not` inside the operand, so the
            // wide edit can't leak surface operators
            let new_node = lower(expr, &mut self.needs);
            self.edits.push((u.range, render_expr(&new_node)));
            return Recurse::Stop;
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
    fn simple_not_annotation() {
        check(
            "a: not int\n",
            indoc! {"
                from ty_extensions import Not
                a: Not[int]
            "},
        );
    }

    #[test]
    fn not_in_subscript() {
        check(
            "a: list[not int]\n",
            indoc! {"
                from ty_extensions import Not
                a: list[Not[int]]
            "},
        );
    }

    #[test]
    fn not_in_union() {
        // python precedence: `not int | str` parses as `not (int | str)`
        // (BitOr binds tighter than not). Result: `Not[int | str]`
        check(
            "a: not int | str\n",
            indoc! {"
                from ty_extensions import Not
                a: Not[int | str]
            "},
        );
    }

    #[test]
    fn parenthesized_inner() {
        // explicit parens around the union arm — same result as
        // not_in_union but the input source carries the parentheses
        check(
            "a: not (int | str)\n",
            indoc! {"
                from ty_extensions import Not
                a: Not[int | str]
            "},
        );
    }

    #[test]
    fn value_position_unchanged_literal() {
        // `not <constant>` in value context — boolean negation, leave alone
        unchanged("x = not True\n");
    }

    #[test]
    fn not_in_function_signature() {
        check(
            "def f(x: not int) -> not str: ...\n",
            indoc! {"
                from ty_extensions import Not
                def f(x: Not[int]) -> Not[str]: ...
            "},
        );
    }

    #[test]
    fn value_not_unchanged() {
        unchanged("x = not y\n");
    }

    #[test]
    fn not_in_type_alias_rhs() {
        // PY312+ keeps `type X = …` native; on PY310 the generics polyfill
        // would rewrite the whole statement and subsume this edit
        check_py312(
            "type X = not int\n",
            indoc! {"
                from ty_extensions import Not
                type X = Not[int]
            "},
        );
    }

    #[test]
    fn not_in_typeparam_bound() {
        check_py312(
            "def f[T: not int](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Not
                def f[T: Not[int]](x: T) -> T: ...
            "},
        );
    }

    #[test]
    fn not_in_typeparam_default() {
        check_py312(
            "def f[T = not int](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Not
                def f[T = Not[int]](x: T) -> T: ...
            "},
        );
    }

    #[test]
    fn not_in_class_base() {
        check(
            "class C(list[not int]): ...\n",
            indoc! {"
                from ty_extensions import Not
                class C(list[Not[int]]): ...
            "},
        );
    }

    #[test]
    fn not_in_value_position_type_application() {
        // `list[not int]` used as a value expression (e.g. passed to
        // `reveal_type`) — still a type application, should be lowered
        check(
            "reveal_type(list[not int])\n",
            indoc! {"
                from ty_extensions import Not
                reveal_type(list[Not[int]])
            "},
        );
    }

    #[test]
    fn not_in_cast_first_arg() {
        check(
            "from typing import cast\nb = cast(not int, a)\n",
            indoc! {"
                from ty_extensions import Not
                from typing import cast
                b = cast(Not[int], a)
            "},
        );
    }

    #[test]
    fn not_in_callable_param_and_return() {
        check(
            "from typing import Callable\nf: Callable[[not int], not str]\n",
            indoc! {"
                from ty_extensions import Not
                from typing import Callable
                f: Callable[[Not[int]], Not[str]]
            "},
        );
    }

    #[test]
    fn not_in_annotated_first_arg_only() {
        // `Annotated[T, meta]` — only first arg is a type position. metadata
        // is arbitrary value text and must not be touched
        check(
            "from typing import Annotated\na: Annotated[not int, \"doc\"]\n",
            indoc! {"
                from ty_extensions import Not
                from typing import Annotated
                a: Annotated[Not[int], \"doc\"]
            "},
        );
    }

    #[test]
    fn not_inside_literal_opaque() {
        // `Literal[True, False]` is opaque — its slice elements are not
        // type expressions. don't descend (would try to wrap booleans)
        unchanged("from typing import Literal\na: Literal[True, False]\n");
    }

    #[test]
    fn not_of_intersection_operand_lowered() {
        // the operand lowers through the shared intersection/union lowering
        // so `&` doesn't leak into the emitted python
        check(
            "a: not (A & B)\n",
            indoc! {"
                from ty_extensions import Intersection, Not
                a: Not[Intersection[A, B]]
            "},
        );
    }

    #[test]
    fn not_of_or_keyword_operand_lowered() {
        check(
            "a: not (A or B)\n",
            indoc! {"
                from ty_extensions import Not
                a: Not[A | B]
            "},
        );
    }

    #[test]
    fn nested_not_lowered() {
        check(
            "a: not not int\n",
            indoc! {"
                from ty_extensions import Not
                a: Not[Not[int]]
            "},
        );
    }

    #[test]
    fn nested_not_in_dict_slice() {
        // unparenthesized tuple inside subscript slice — both elements are
        // type positions, both should descend
        check(
            "a: dict[not int, not str]\n",
            indoc! {"
                from ty_extensions import Not
                a: dict[Not[int], Not[str]]
            "},
        );
    }
}
