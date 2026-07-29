//! Lowers the one use-site type modifier Python can spell: `literal str` →
//! `LiteralString`.
//!
//! The other modifiers ([`literal T`](super::use_site_variance) on a non-`str`
//! type, `final T`) have no stdlib spelling and are erased with the rest of the
//! keyword-prefix markers. `literal str` is different: it denotes exactly the
//! set `typing.LiteralString` does — ty reduces it to that very type on
//! construction — so lowering it to the stdlib name keeps the literal-ness
//! readable by whatever checks the produced Python, and lets the
//! [reverse transform](crate::reverse_transforms::literal_string) put the
//! keyword back.
//!
//! This is a *collector*, not an [`AstPass`](super::ast_driver): the markers are
//! blanked out of the source before the passes run, so by the time a pass walks
//! the AST there is no `literal` left to see. It runs instead over the db's own
//! parse — which keeps the original, marker-bearing source — exactly like
//! [`symbolic_type_op::collect_symbolic_folds`](super::symbolic_type_op::collect_symbolic_folds).
//!
//! Reading the *marker's* inferred type, rather than matching the spelling of
//! the type it wraps, is what makes this agree with the checker by construction:
//! a shadowed `str` never reduces to `LiteralString`, so it never rewrites here.

use ruff_python_ast::helpers::{TypeModifier, type_modifier_marker};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};
use crate::type_info::TypeInfo;

/// The `literal str` markers to rewrite, and whether the import is needed.
#[derive(Default)]
pub(crate) struct LiteralStringRewrites {
    /// each marker's full source range — the keyword through the type it wraps
    ranges: Vec<TextRange>,
    /// whether any rewrite fired *and* `LiteralString` is not already bound, so
    /// the driver can add the import
    pub(crate) needs_import: bool,
}

impl LiteralStringRewrites {
    /// the `(range, replacement)` edits the driver applies
    pub(crate) fn edits(&self) -> impl Iterator<Item = (TextRange, String)> + '_ {
        self.ranges
            .iter()
            .map(|range| (*range, "LiteralString".to_owned()))
    }

    /// whether `range` falls inside a rewritten marker. The blanking pass emits
    /// a collapse edit for every marker keyword; for a marker rewritten here
    /// that edit is subsumed, and letting both through would leave two plain
    /// text edits racing for the same start offset
    pub(crate) fn covers(&self, range: TextRange) -> bool {
        self.ranges
            .iter()
            .any(|rewritten| rewritten.contains_range(range))
    }
}

/// Collect every `literal str` marker in `stmts` that ty reduced to
/// `LiteralString`. `stmts` must come from the same parse `types` answers for.
pub(crate) fn collect(stmts: &[Stmt], types: &dyn TypeInfo) -> LiteralStringRewrites {
    let mut collector = Collector {
        types,
        ranges: Vec::new(),
    };
    walk_type_positions(stmts, Some(types), &mut collector);
    let needs_import = !collector.ranges.is_empty() && !types.is_bound_globally("LiteralString");
    LiteralStringRewrites {
        ranges: collector.ranges,
        needs_import,
    }
}

struct Collector<'a> {
    types: &'a dyn TypeInfo,
    ranges: Vec<TextRange>,
}

impl TypeExprVisitor for Collector<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Some((TypeModifier::Literal, _)) = type_modifier_marker(expr)
            && self.types.is_literal_string(expr)
        {
            self.ranges.push(expr.range());
            // the wrapped type is `str`; nothing inside it can carry another
            // modifier, and descending would let a nested walk re-claim part of
            // the range this edit already covers
            return Recurse::Stop;
        }
        Recurse::Descend
    }
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
    fn simple_annotation() {
        check(
            "a: literal str = \"x\"\n",
            indoc! {"
                from typing_extensions import LiteralString
                a: LiteralString = \"x\"
            "},
        );
    }

    #[test]
    fn nested_and_union_positions() {
        check(
            "a: list[literal str]\n",
            indoc! {"
                from typing_extensions import LiteralString
                a: list[LiteralString]
            "},
        );
        check(
            "b: literal str | None\n",
            indoc! {"
                from typing_extensions import LiteralString
                b: LiteralString | None
            "},
        );
    }

    #[test]
    fn parameter_and_return_positions() {
        check(
            "def f(x: literal str) -> literal str: ...\n",
            indoc! {"
                from typing_extensions import LiteralString
                def f(x: LiteralString) -> LiteralString: ...
            "},
        );
    }

    #[test]
    fn other_modifiers_still_erase() {
        // only `literal str` has a stdlib spelling; the rest are compile-time
        // only and carry no import
        check("a: literal int = 1\n", "a: int = 1\n");
        check("b: final str = \"x\"\n", "b: str = \"x\"\n");
        check(
            "c: literal list[*] = []\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                c: Top[list[Any]] = []
            "},
        );
    }

    #[test]
    fn already_imported_literal_string_no_duplicate() {
        check(
            "from typing import LiteralString\na: literal str = \"x\"\n",
            indoc! {"
                from typing_extensions import LiteralString
                a: LiteralString = \"x\"
            "},
        );
    }

    #[test]
    fn shadowed_str_is_not_literal_string() {
        // the collector reads the *marker's* inferred type, so a shadowed `str`
        // never reduces to `LiteralString` and the modifier just erases
        check(
            indoc! {"
                class str: ...
                a: literal str
            "},
            indoc! {"
                class str: ...
                a: str
            "},
        );
    }

    #[test]
    fn plain_python_untouched() {
        unchanged("a: str = \"x\"\n");
    }
}
