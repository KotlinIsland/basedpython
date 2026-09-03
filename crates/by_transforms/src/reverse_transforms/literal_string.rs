//! reverse of `crate::transforms::literal_string`:
//!   `LiteralString`        → `literal str`
//!   `typing.LiteralString` → `literal str`
//!
//! only fires in type positions on a name spelled `LiteralString` that also
//! *resolves* to `LiteralString` — the same pair of guards
//! [`dynamic_keyword`](super::dynamic_keyword) uses, so an alias
//! (`MyStr = LiteralString; x: MyStr`) keeps its own spelling and a shadowing
//! binding is left alone.
//!
//! descent into nested type positions (`list[LiteralString]`,
//! `LiteralString | None`, `Annotated[LiteralString, meta]` first arg) is
//! delegated to [`type_expr_walker`], so metadata and `Literal[…]` slices are
//! never touched.
//!
//! once the last reference is rewritten, `from typing import LiteralString` is
//! dead and [`prune_imports`](super::prune_imports) drops it.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::Ranged;

use crate::transforms::source_util::for_each_annotation_in_stmt;
use crate::transforms::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_one_type_expr};
use crate::type_info::TypeInfo;

pub(crate) struct LiteralStringReverse<'src> {
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> LiteralStringReverse<'src> {
    pub(crate) fn new(types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
        }
    }

    /// `LiteralString` (bare) or `<mod>.LiteralString`, where the name is
    /// spelled `LiteralString` *and* resolves to it
    fn is_literal_string(&self, expr: &Expr) -> bool {
        let spelled = match expr {
            Expr::Name(name) => name.id.as_str() == "LiteralString",
            Expr::Attribute(attribute) => attribute.attr.id.as_str() == "LiteralString",
            _ => false,
        };
        spelled && self.types.is_literal_string(expr)
    }

    fn rewrite_annotation(&mut self, annotation: &Expr) {
        walk_one_type_expr(annotation, self);
    }
}

impl TypeExprVisitor for LiteralStringReverse<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if self.is_literal_string(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                "literal str".to_owned(),
                expr.range(),
            )));
        }
        Recurse::Descend
    }
}

impl<'ast> Visitor<'ast> for LiteralStringReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        for_each_annotation_in_stmt(stmt, |annotation| {
            self.rewrite_annotation(annotation);
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};
    use indoc::indoc;

    /// `Config::test_default` leaves import pruning off so expectations stay
    /// stable; the now-dead `from typing import LiteralString` is dropped in
    /// real `--reverse` usage, which `dead_import_is_pruned` covers
    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn simple_annotation() {
        check(
            "from typing import LiteralString\nx: LiteralString\n",
            "from typing import LiteralString\nx: literal str\n",
        );
    }

    #[test]
    fn nested_in_generic() {
        check(
            "from typing import LiteralString\nx: list[LiteralString]\n",
            "from typing import LiteralString\nx: list[literal str]\n",
        );
    }

    #[test]
    fn in_union() {
        check(
            "from typing import LiteralString\nx: LiteralString | None\n",
            "from typing import LiteralString\nx: literal str?\n",
        );
    }

    #[test]
    fn function_param_and_return() {
        check(
            indoc! {"
                from typing import LiteralString
                def f(x: LiteralString) -> LiteralString: ...
            "},
            indoc! {"
                from typing import LiteralString
                def f(x: literal str) -> literal str
            "},
        );
    }

    #[test]
    fn qualified_typing_literal_string() {
        check(
            indoc! {"
                import typing
                x: typing.LiteralString
            "},
            indoc! {"
                import typing
                x: literal str
            "},
        );
    }

    #[test]
    fn annotated_first_arg_only() {
        // only the first arg of `Annotated[T, meta]` is a type position; a
        // `LiteralString` in the metadata slot is an arbitrary value — it comes
        // back verbatim as the decorator the `Annotated` reverses into
        check(
            "from typing import Annotated, LiteralString\nx: Annotated[LiteralString, LiteralString]\n",
            "from typing import Annotated, LiteralString\nx: @LiteralString literal str\n",
        );
    }

    #[test]
    fn value_position_unchanged() {
        check(
            "from typing import LiteralString\nx = LiteralString\n",
            "from typing import LiteralString\nx = LiteralString\n",
        );
    }

    #[test]
    fn shadowed_unchanged() {
        // a local binding shadows the typing import, so the name no longer
        // resolves to `LiteralString` — leave it alone
        check(
            indoc! {"
                LiteralString = object()
                x: LiteralString
            "},
            indoc! {"
                LiteralString = object()
                x: LiteralString
            "},
        );
    }

    #[test]
    fn dead_import_is_pruned() {
        // the real `--reverse` path prunes: once the last reference is a
        // keyword, the import supplies nothing
        let out = reverse_transpile(
            "from typing import LiteralString\nx: LiteralString\n",
            &Config::default(),
        )
        .unwrap();
        assert_eq!(out, "x: literal str\n");
    }

    /// the pair is a real round trip: python in, the same python out
    #[test]
    fn round_trips_through_the_forward_lowering() {
        let python = "from typing import LiteralString\nx: LiteralString\n";
        let reversed = reverse_transpile(python, &Config::default()).unwrap();
        assert_eq!(reversed, "x: literal str\n");
        let forward = transpile(&reversed, &Config::test_default()).unwrap();
        assert!(
            forward.contains("x: LiteralString"),
            "expected the annotation back, got:\n{forward}"
        );
    }
}
