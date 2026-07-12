//! End-to-end tests for `not T` in type positions (`a: not int` →
//! `a: Not[int]`). The lowering is performed by the single type-expression
//! lowerer in [`callable`](super::callable); this module only hosts the tests.

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
        // native passthrough of a defaulted list needs 3.13 (pep 696)
        check_at(
            "def f[T = not int](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Not
                def f[T = Not[int]](x: T) -> T: ...
            "},
            PythonVersion::PY313,
        );
    }

    #[test]
    fn not_in_typeparam_default_downlevels_on_312() {
        // on a 3.12 target the defaulted list polyfills, and the negation
        // still lowers inside the `default=` argument
        check_py312(
            "def f[T = not int](x: T) -> T: ...\n",
            indoc! {"
                from ty_extensions import Not
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", default=Not[int])
                def f(x: _T) -> _T: ...
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

    #[test]
    fn not_arm_float_composes() {
        // the `float` operand must still lower to `JustFloat` through the
        // template's `Src` passthrough — the wide `Not[…]` edit used to swallow
        // just_float's narrow edit while keeping its (now-orphan) import
        check(
            "a: not float\n",
            indoc! {"
                from ty_extensions import JustFloat, Not
                a: Not[JustFloat]
            "},
        );
    }
}
