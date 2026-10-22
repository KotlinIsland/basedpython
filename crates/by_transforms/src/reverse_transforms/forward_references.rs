//! reverse of the forward-reference quoting in [`crate::transforms::auto_quote`]
//!
//! a string annotation is a forward reference in python, and in basedpython it
//! would be a string-literal *type*: `def f() -> "Plain"` reads as returning the
//! string `"Plain"`, and transpiles back as `Literal["Plain"]`. basedpython
//! resolves every annotation as deferred and the forward transpile quotes each
//! reference that needs it, so the reverse writes out the expression the string
//! spells: `-> Plain`
//!
//! this runs over the source *before* the other reverse transforms, whose edits
//! all key on one parse of it. unquoted first, `"Plain | None"` is a real
//! union by the time the optional transform looks for one to write `Plain?`. a string
//! inside the unquoted text is a forward reference too, and is unquoted the same
//! way
//!
//! the positions are the ones where basedpython reads a string as a literal type, which
//! the forward transpile writes as `Literal[…]`: the very strings that lowering wraps
//! ([`literal_type_strings`]), so that a forward reference python wrote comes back from a
//! round trip as the reference it was. a string left a string here would come back a
//! `Literal`. inside the unquoted text a string is a forward reference in every type
//! position but the arguments of `Literal[…]` and the metadata of `Annotated[…]`. a
//! string whose text is not a single expression, or is a bare tuple — which means
//! something else once it is not a string — is left as it is

use ruff_python_ast::Expr;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_parser::parse_expression;
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::literal_types::literal_type_strings;
use crate::transforms::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_one_type_expr};

/// `source` with each string the forward transpile would write as a literal type replaced by
/// the expression it spells
pub(crate) fn unquote_forward_references(source: &str) -> String {
    let (db, file) = crate::make_in_memory_db(source);
    let program_file = ty_python_semantic::Db::program_file(&db, file);
    let module = ruff_db::parsed::parsed_module(&db, program_file.python_file(&db)).load(&db);
    if !module.errors().is_empty() {
        return source.to_owned();
    }
    let model = ty_python_semantic::LoweringModel::new(&db, program_file);
    let ranges = literal_type_strings(source, module.suite(), &model);
    let mut strings = Strings {
        ranges: &ranges,
        collector: Collector { edits: Vec::new() },
    };
    strings.visit_body(module.suite());
    splice(source, strings.collector.edits)
}

/// the strings at `ranges`, each unquoted as a forward reference
struct Strings<'a> {
    ranges: &'a [TextRange],
    collector: Collector,
}

impl<'ast> SourceOrderVisitor<'ast> for Strings<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if expr.is_string_literal_expr() && self.ranges.contains(&expr.range()) {
            self.collector.visit(expr, TypePos::Root);
        } else {
            walk_expr(self, expr);
        }
    }
}

struct Collector {
    edits: Vec<(TextRange, String)>,
}

impl TypeExprVisitor for Collector {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        let Expr::StringLiteral(string) = expr else {
            return Recurse::Descend;
        };
        // an implicitly concatenated string is one value spread over several
        // literals, and is left as the author wrote it
        if !string.value.is_implicit_concatenated()
            && let Some(expression) = spelled_expression(string.value.to_str())
        {
            self.edits.push((expr.range(), expression));
        }
        Recurse::Stop
    }
}

/// the expression a forward reference's text spells, with the forward
/// references inside it unquoted too, or `None` when it is not one the
/// annotation can hold in its place
fn spelled_expression(text: &str) -> Option<String> {
    let text = text.trim();
    let parsed = parse_expression(text).ok()?;
    let expression = parsed.expr();
    if let Expr::Tuple(tuple) = expression
        && !tuple.parenthesized
    {
        return None;
    }
    let mut collector = Collector { edits: Vec::new() };
    walk_one_type_expr(expression, &mut collector);
    let unquoted = splice(text, collector.edits);
    // the string stood as one operand wherever it was written. a name, a
    // generic, an attribute or a union keeps that shape without help; anything
    // looser, or spread over lines, is parenthesized to hold together
    let holds_together = matches!(
        expression,
        Expr::Name(_)
            | Expr::Attribute(_)
            | Expr::Subscript(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
            | Expr::List(_)
            | Expr::Tuple(_)
    ) || matches!(expression, Expr::BinOp(binop) if binop.op.is_bit_or());
    Some(if holds_together && !unquoted.contains('\n') {
        unquoted
    } else {
        format!("({unquoted})")
    })
}

fn splice(source: &str, mut edits: Vec<(TextRange, String)>) -> String {
    edits.sort_by_key(|(range, _)| range.start());
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (range, text) in edits {
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&text);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn a_string_annotation_is_the_expression_it_spells() {
        check(
            indoc! {"
                def later() -> \"Later\":
                    return Later()


                class Later: ...
            "},
            indoc! {"
                def later() -> Later:
                    return Later()


                class Later
            "},
        );
    }

    #[test]
    fn a_self_reference_in_a_method_signature() {
        check(
            indoc! {"
                class Plain:
                    def m(self, other: \"Plain\") -> \"Plain\":
                        return self
            "},
            indoc! {"
                class Plain:
                    def m(self, other: Plain) -> Plain:
                        return self
            "},
        );
    }

    #[test]
    fn a_string_inside_a_generic() {
        check("x: list[\"Later\"] = []\n", "x: list[Later] = []\n");
    }

    /// the text of a string annotation can hold forward references of its own
    #[test]
    fn a_string_inside_the_unquoted_text() {
        check(
            "x: \"dict[str, 'Later']\" = {}\n",
            "x: dict[str, Later] = {}\n",
        );
    }

    /// unquoted before the other transforms run, the text is a real annotation
    /// by the time they look at it
    #[test]
    fn the_unquoted_text_is_reversed_as_well() {
        check(
            indoc! {"
                def f(x: \"Later | None\") -> None: ...


                class Later: ...
            "},
            indoc! {"
                def f(x: Later?) -> None: ...


                class Later
            "},
        );
    }

    /// `Literal`'s arguments and `Annotated`'s metadata are values, not types
    #[test]
    fn literal_arguments_and_annotated_metadata_stay_strings() {
        check(
            indoc! {"
                from typing import Annotated


                x: Annotated[int, \"doc\"] = 1
            "},
            indoc! {"
                from typing import Annotated


                x: Annotated[int, \"doc\"] = 1
            "},
        );
    }

    #[test]
    fn a_local_variable_annotation() {
        check(
            indoc! {"
                def f() -> None:
                    x: \"Later\" = Later()
            "},
            indoc! {"
                def f() -> None:
                    x: Later = Later()
            "},
        );
    }

    /// every string the forward transpile would write as a literal type is a forward reference
    /// in python, wherever it stands: a type argument in a value expression, a base's, a typing
    /// construct's and a type variable's bound among them
    #[test]
    fn a_string_the_forward_transpile_reads_as_a_literal_type_is_unquoted() {
        check(
            indoc! {"
                from typing import Callable, Generic, List, Optional, TypeVar, Union

                T = TypeVar(\"T\")

                class Foo(Generic[T]):
                    pass

                class Later:
                    pass

                Alias = List[\"Later\"]
                UAlias = Union[\"Later\", int]
                CAlias = Callable[[\"Later\"], \"Later\"]
                MaybeLater = Optional[\"Later\"]
                B = TypeVar(\"B\", bound=\"Later\")

                class Bar(Foo[\"Later\"]):
                    pass
            "},
            indoc! {"
                from typing import Callable, Generic, List, Optional, TypeVar, Union

                T = TypeVar(\"T\")

                class Foo(Generic[T]):
                    pass

                class Later:
                    pass

                Alias = List[Later]
                UAlias = Union[Later, int]
                CAlias = Callable[[Later], Later]
                MaybeLater = Optional[Later]
                B = TypeVar(\"B\", bound=Later)

                class Bar(Foo[Later]):
                    pass
            "},
        );
    }

    /// the value of a `TypeAlias` is a type expression
    #[test]
    fn a_type_alias_value_is_unquoted() {
        check(
            "from typing import TypeAlias\n\nclass Later: ...\n\nX: TypeAlias = \"Later\"\n",
            "from typing import TypeAlias\n\nclass Later\n\ntype X = Later\n",
        );
    }

    /// a string that is no type expression — a subscript on a class that is not generic, an
    /// enum's member lookup — is a value, and stays one
    #[test]
    fn a_member_lookup_stays_a_string() {
        let source =
            "from enum import Enum\n\nclass Colour(Enum):\n    Red = 1\n\nred = Colour[\"Red\"]\n";
        let out = reverse_transpile(source, &Config::test_default()).unwrap();
        assert!(out.contains("red = Colour[\"Red\"]"), "{out}");
    }

    /// a bare tuple means something else once it is not a string
    #[test]
    fn a_bare_tuple_stays_a_string() {
        check("x: \"int, str\" = 1\n", "x: \"int, str\" = 1\n");
    }

    /// the forward transpile quotes whatever the reverse unquoted wherever
    /// python evaluates annotations as the definition runs
    #[test]
    fn the_reference_is_quoted_again_on_the_way_back() {
        let python = indoc! {"
            def later() -> \"Later\":
                return Later()


            class Later: ...
        "};
        let by = reverse_transpile(python, &Config::test_default()).unwrap();
        let back = transpile(&by, &Config::test_default()).unwrap();
        assert!(back.contains("def later() -> \"Later\":"), "{back}");
    }
}
