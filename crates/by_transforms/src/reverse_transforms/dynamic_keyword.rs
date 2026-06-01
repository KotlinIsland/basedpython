//! reverse of `crate::transforms::dynamic_keyword`:
//!   `Any`        → `dynamic`
//!   `typing.Any` → `dynamic`
//!
//! only fires in annotation positions when `Any` resolves to a type context
//! (the typing special form, or an unresolved import) — a local `Any = …`
//! binding is left alone. descent into nested type positions (`list[Any]`,
//! `Any | None`, `Callable[[Any], Any]`, `Annotated[Any, meta]` first arg) is
//! delegated to [`type_expr_walker`], so metadata and `Literal[…]` slices are
//! never touched.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::Ranged;

use crate::transforms::source_util::for_each_annotation_in_stmt;
use crate::transforms::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_one_type_expr};
use crate::type_info::TypeInfo;

pub(crate) struct DynamicKeywordReverse<'src> {
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> DynamicKeywordReverse<'src> {
    pub(crate) fn new(types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
        }
    }

    /// `Any` (bare) or `<mod>.Any` (`typing.Any`, `t.Any`), where the name is
    /// spelled `Any` *and* resolves to the `typing.Any` special form. the
    /// spelling guard avoids rewriting an alias (`MyAny = Any; x: MyAny`) that
    /// also resolves to `Any`; the resolution guard avoids a shadowed
    /// `Any = object()` (which resolves to `Unknown`, not `Any`)
    fn is_any(&self, expr: &Expr) -> bool {
        let spelled_any = match expr {
            Expr::Name(n) => n.id.as_str() == "Any",
            Expr::Attribute(a) => a.attr.id.as_str() == "Any",
            _ => false,
        };
        spelled_any && self.types.is_any(expr)
    }

    fn rewrite_annotation(&mut self, ann: &Expr) {
        walk_one_type_expr(ann, self);
    }
}

impl TypeExprVisitor for DynamicKeywordReverse<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if self.is_any(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                "dynamic".to_owned(),
                expr.range(),
            )));
        }
        Recurse::Descend
    }
}

impl<'ast> Visitor<'ast> for DynamicKeywordReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        for_each_annotation_in_stmt(stmt, |ann| {
            self.rewrite_annotation(ann);
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn simple_annotation() {
        check(
            "from typing import Any\nx: Any\n",
            "from typing import Any\nx: dynamic\n",
        );
    }

    #[test]
    fn return_type() {
        check(
            indoc! {"
                from typing import Any
                def f() -> Any: ...
            "},
            indoc! {"
                from typing import Any
                def f() -> dynamic
            "},
        );
    }

    #[test]
    fn nested_in_generic() {
        check(
            "from typing import Any\nx: list[Any]\n",
            "from typing import Any\nx: list[dynamic]\n",
        );
    }

    #[test]
    fn nested_in_dict() {
        check(
            "from typing import Any\nx: dict[str, Any]\n",
            "from typing import Any\nx: dict[str, dynamic]\n",
        );
    }

    #[test]
    fn in_union() {
        check(
            "from typing import Any\nx: Any | None\n",
            "from typing import Any\nx: dynamic | None\n",
        );
    }

    #[test]
    fn function_param() {
        check(
            indoc! {"
                from typing import Any
                def f(x: Any) -> None: ...
            "},
            indoc! {"
                from typing import Any
                def f(x: dynamic) -> None
            "},
        );
    }

    #[test]
    fn qualified_typing_any() {
        check(
            indoc! {"
                import typing
                x: typing.Any
            "},
            indoc! {"
                import typing
                x: dynamic
            "},
        );
    }

    #[test]
    fn annotated_first_arg_only() {
        // only the first arg of `Annotated[T, meta]` is a type position; an
        // `Any` in the metadata slot is an arbitrary value, leave it alone
        check(
            "from typing import Annotated, Any\nx: Annotated[Any, Any]\n",
            "from typing import Annotated, Any\nx: Annotated[dynamic, Any]\n",
        );
    }

    #[test]
    fn value_position_unchanged() {
        // `Any` outside an annotation isn't touched (conservative reverse)
        check(
            "from typing import Any\nx = Any\n",
            "from typing import Any\nx = Any\n",
        );
    }

    #[test]
    fn shadowed_unchanged() {
        // local binding shadows the typing import — don't rewrite
        check(
            indoc! {"
                Any = object()
                x: Any
            "},
            indoc! {"
                Any = object()
                x: Any
            "},
        );
    }
}
