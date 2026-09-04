//! Lowers a template literal type — an f-string in a type position — to the
//! nearest python spelling.
//!
//! ```by
//! path: f"/{str}"
//! kind: f"a{1 | 2}b"
//! ```
//!
//! →
//!
//! ```python
//! path: str
//! kind: Literal["a1b", "a2b"]
//! ```
//!
//! python has no template literal types, so a pattern that is still a pattern
//! widens to `str` — every string it produces is one. a pattern whose holes all
//! folded to literal text is a finite set of strings, and that precision is
//! worth keeping: it is spelled as the `Literal[…]` the checker already resolved
//! it to.
//!
//! the type is read back from ty rather than recomputed here, so the emitted
//! annotation cannot disagree with what the checker decided. an f-string ty
//! could not resolve still lowers to `str`: an f-string annotation is not valid
//! python whatever it meant, so leaving it alone is not an option.
//!
//! there is no reverse transform. the lowering is lossy in the direction that
//! matters — a `str` annotation carries no evidence that it was ever a pattern,
//! and a `Literal[…]` of several strings has many patterns that produce it and
//! no reason to prefer one. reading a pattern back out would be a guess, so the
//! python → basedpython direction leaves both spellings alone.

use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping,
};
use crate::type_info::TypeInfo;

pub(crate) struct TemplateTypePass;

impl TypeAwarePass for TemplateTypePass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = TemplateType {
            types,
            edits: Vec::new(),
            needs_literal_import: false,
        };
        walk_type_positions_skipping(stmts, Some(types), &ctx.claimed_type_op_ranges, &mut inner);
        if inner.needs_literal_import {
            ctx.required_imports
                .push("from typing import Literal".to_owned());
        }
        ctx.text_edits.extend(inner.edits);
    }
}

struct TemplateType<'src> {
    types: &'src dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
    needs_literal_import: bool,
}

impl TypeExprVisitor for TemplateType<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        let Expr::FString(_) = expr else {
            return Recurse::Descend;
        };
        let replacement = match self.types.template_literal_strings(expr) {
            Some(strings) if !strings.is_empty() => {
                self.needs_literal_import = true;
                let arms = strings
                    .iter()
                    .map(|value| render_string_literal(value))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Literal[{arms}]")
            }
            _ => "str".to_owned(),
        };
        self.edits.push((expr.range(), replacement));
        // the holes are type expressions of their own, but they are gone from
        // the output — descending would let another pass emit an edit inside a
        // span this one has already replaced
        Recurse::Stop
    }
}

/// spell `value` as a python string literal, preferring double quotes
fn render_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(character),
        }
    }
    out.push('"');
    out
}

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
    fn pattern_widens_to_str() {
        check("a: f\"asdf{int}fdsa\"\n", "a: str\n");
    }

    #[test]
    fn folded_pattern_keeps_its_strings() {
        check(
            "a: f\"a{1 | 2}b\"\n",
            indoc! {"
                from typing import Literal
                a: Literal[\"a1b\", \"a2b\"]
            "},
        );
    }

    #[test]
    fn lone_string_hole_is_str() {
        check("a: f\"{str}\"\n", "a: str\n");
    }

    #[test]
    fn nested_in_a_subscript() {
        check("a: list[f\"v{int}\"]\n", "a: list[str]\n");
    }

    #[test]
    fn union_arm() {
        check("a: f\"v{int}\" | None\n", "a: str | None\n");
    }

    #[test]
    fn parameter_and_return() {
        check(
            "def f(p: f\"/{str}\") -> f\"v{int}\": ...\n",
            "def f(p: str) -> str: ...\n",
        );
    }

    #[test]
    fn value_position_fstring_is_untouched() {
        check("a = f\"v{1}\"\n", "a = f\"v{1}\"\n");
    }

    #[test]
    fn quotes_in_the_folded_text_are_escaped() {
        check(
            "a: f'{\"q\"}\"x\"'\n",
            "from typing import Literal\na: Literal[\"q\\\"x\\\"\"]\n",
        );
    }

    #[test]
    fn type_alias_rhs() {
        check(
            "type X = f\"v{int}\"\n",
            indoc! {"
                from typing_extensions import TypeAliasType
                X = TypeAliasType(\"X\", str)
            "},
        );
    }

    #[test]
    fn a_type_alias_hole_folds() {
        check(
            "type Name = \"foo\" | \"bar\"\ntype Title = f\"the {Name}\"\n",
            indoc! {"
                from typing import Literal
                from typing_extensions import TypeAliasType
                Name = TypeAliasType(\"Name\", Literal[\"foo\", \"bar\"])
                Title = TypeAliasType(\"Title\", Literal[\"the foo\", \"the bar\"])
            "},
        );
    }

    #[test]
    fn an_alias_of_an_alias_hole_folds() {
        check(
            "type Inner = \"foo\" | \"bar\"\ntype Outer = Inner | \"baz\"\ntype Title = f\"the {Outer}\"\n",
            indoc! {"
                from typing import Literal
                from typing_extensions import TypeAliasType
                Inner = TypeAliasType(\"Inner\", Literal[\"foo\", \"bar\"])
                Outer = TypeAliasType(\"Outer\", Inner | Literal[\"baz\"])
                Title = TypeAliasType(\"Title\", Literal[\"the foo\", \"the bar\", \"the baz\"])
            "},
        );
    }
}
