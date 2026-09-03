//! End-to-end tests for a decorated type — `a: @meta int` → `a: Annotated[int, meta]`.
//! The lowering is performed by the single type-expression lowerer in
//! [`callable`](super::callable); this module only hosts the tests.

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple_annotation() {
        check(
            "a: @meta int\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, meta]
            "},
        );
    }

    #[test]
    fn decorator_with_arguments() {
        check(
            "a: @field(gt=0) int\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, field(gt=0)]
            "},
        );
    }

    #[test]
    fn dotted_decorator() {
        check(
            "a: @mod.meta int\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, mod.meta]
            "},
        );
    }

    #[test]
    fn nested_in_a_subscript() {
        check(
            "a: list[@meta int]\n",
            indoc! {"
                from typing import Annotated
                a: list[Annotated[int, meta]]
            "},
        );
    }

    #[test]
    fn takes_the_whole_type_written_after_it() {
        // unlike the `literal` / `final` use-site modifiers, which take the operand
        // next to them and nothing more, a decoration runs to the end of the type —
        // so the union is what is decorated
        check(
            "a: @meta int | None\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int | None, meta]
            "},
        );
    }

    #[test]
    fn one_arm_of_a_union_has_to_be_grouped() {
        // the group is what stops the decoration running on to take `str` as well
        check(
            "a: (@meta int) | str\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, meta] | str
            "},
        );
    }

    #[test]
    fn a_chain_collapses_into_one_annotated() {
        // decorators apply bottom-up, so the metadata reads in that order
        check(
            "a: @x @y int\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, y, x]
            "},
        );
    }

    #[test]
    fn parameter_and_return_annotations() {
        check(
            "def f(x: @meta int) -> @meta str: ...\n",
            indoc! {"
                from typing import Annotated
                def f(x: Annotated[int, meta]) -> Annotated[str, meta]: ...
            "},
        );
    }

    #[test]
    fn on_a_let_declaration() {
        check(
            "let b: @meta int = 1\n",
            indoc! {"
                from typing import Annotated, Final
                b: Final[Annotated[int, meta]] = 1
            "},
        );
    }

    #[test]
    fn composes_with_a_lowering_inside_the_type() {
        check(
            "a: @meta list[int?]\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[list[int | None], meta]
            "},
        );
    }

    #[test]
    fn a_postfix_optional_is_part_of_the_decorated_type() {
        // `?` is read at the same level as `|`, and the decoration runs past both —
        // so the optional is what is decorated, not the other way round
        check(
            "a: @meta int?\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int | None, meta]
            "},
        );
    }

    #[test]
    fn a_grouped_decoration_is_what_a_postfix_optional_applies_to() {
        // the lowering edits the source in place, so the group the author wrote to
        // hold the decoration together is still standing around it afterwards
        check(
            "a: (@meta int)?\n",
            indoc! {"
                from typing import Annotated
                a: (Annotated[int, meta]) | None
            "},
        );
    }

    #[test]
    fn a_parenthesized_type_is_the_decorated_one() {
        // the decorator is read greedily, so `meta (int | None)` first reads as a
        // call — with nothing left to decorate. the group is the type instead
        check(
            "a: @meta (int | None)\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int | None, meta]
            "},
        );
    }

    #[test]
    fn a_call_still_wins_when_a_type_follows_it() {
        check(
            "a: @field (gt=0) int\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, field (gt=0)]
            "},
        );
    }

    #[test]
    fn in_a_type_alias() {
        check(
            "type X = @meta int\n",
            indoc! {r#"
                from typing import Annotated
                from typing_extensions import TypeAliasType
                X = TypeAliasType("X", Annotated[int, meta])
            "#},
        );
    }

    #[test]
    fn a_decorated_type_on_a_decorated_binding() {
        // both spellings mean metadata on the same type, so both land on it. the
        // nesting is what `Annotated` flattens on its own — `Annotated[int, meta, foo]`
        // is what the two of them add up to
        check(
            indoc! {"
                @foo
                let b: @meta int = 1
            "},
            indoc! {"
                from typing import Annotated, Final
                b: Final[Annotated[Annotated[int, meta], foo]] = 1
            "},
        );
    }
}
