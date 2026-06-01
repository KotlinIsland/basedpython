//! Rewrites the basedpython `dynamic` keyword to `typing.Any` in type-
//! expression position.
//!
//! basedpython spells the dynamic type `dynamic` rather than importing `Any`:
//!
//! `x: dynamic`          → `x: Any`
//! `def f() -> dynamic`  → `def f() -> Any`
//! `x: list[dynamic]`    → `x: list[Any]`
//!
//! emits a *minimal* edit per occurrence (just the name range), so it composes
//! with overlapping rewrites from `literal_types`, `intersection`, etc. only
//! the unshadowed keyword is rewritten — a local `dynamic = …` binding keeps
//! its identity, mirroring how `just_float` respects a shadowed `float`.
//!
//! traversal is delegated to [`type_expr_walker`], so every type position the
//! walker recognises (annotations, returns, type-alias RHS, type-param
//! bound/default, class bases, value-position type applications, `cast` first
//! arg, `Annotated` first arg, `Callable[[P], R]` param + return) is rewritten
//! consistently with the other type-position transforms

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::Ranged;

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_one_type_expr, walk_type_positions,
};
use crate::type_info::TypeInfo;

pub(crate) struct DynamicKeyword<'src> {
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
    pub(crate) needs_any_import: bool,
}

impl<'src> DynamicKeyword<'src> {
    pub(crate) fn new(types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
            needs_any_import: false,
        }
    }

    /// public so [`crate::transforms::just_float::rewrite_type_expr`] can drive
    /// a one-off lowering over a single expression without spinning up a pass
    /// (used by `generics.rs` when the PEP-695 polyfill replaces a whole type
    /// alias / bound and would otherwise subsume our minimal edits)
    pub(crate) fn emit_in_type_expr(&mut self, expr: &Expr) {
        walk_one_type_expr(expr, self);
    }
}

impl TypeExprVisitor for DynamicKeyword<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::Name(n) = expr
            && n.id.as_str() == "dynamic"
            && self.types.is_unbound_at("dynamic", expr)
        {
            self.needs_any_import = true;
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                "Any".to_owned(),
                n.range(),
            )));
        }
        Recurse::Descend
    }
}

pub(crate) struct DynamicKeywordPass;

impl DynamicKeywordPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for DynamicKeywordPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = DynamicKeyword::new(types);
        walk_type_positions(stmts, Some(types), &mut inner);
        // skip the import when `Any` is already bound at module level (the user
        // imported it themselves) — avoids a duplicate `from typing import Any`
        if inner.needs_any_import && !types.is_bound_globally("Any") {
            ctx.required_imports
                .push("from typing import Any".to_owned());
        }
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
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
    fn simple_annotation() {
        check(
            "a: dynamic\n",
            indoc! {"
                from typing import Any
                a: Any
            "},
        );
    }

    #[test]
    fn annotation_with_value() {
        check(
            "a: dynamic = 1\n",
            indoc! {"
                from typing import Any
                a: Any = 1
            "},
        );
    }

    #[test]
    fn in_union() {
        check(
            "a: dynamic | None\n",
            indoc! {"
                from typing import Any
                a: Any | None
            "},
        );
    }

    #[test]
    fn nested_in_generic() {
        check(
            "a: list[dynamic]\n",
            indoc! {"
                from typing import Any
                a: list[Any]
            "},
        );
    }

    #[test]
    fn nested_in_dict() {
        check(
            "a: dict[str, dynamic]\n",
            indoc! {"
                from typing import Any
                a: dict[str, Any]
            "},
        );
    }

    #[test]
    fn function_param_and_return() {
        check(
            indoc! {"
                def f(x: dynamic) -> dynamic:
                    pass
            "},
            indoc! {"
                from typing import Any
                def f(x: Any) -> Any:
                    pass
            "},
        );
    }

    #[test]
    fn annotated_first_arg_only() {
        // `Annotated[T, meta]` — only the first arg is a type position; the
        // metadata `dynamic` here is an arbitrary value and must be left alone
        check(
            "a: Annotated[dynamic, dynamic]\n",
            indoc! {"
                from typing import Annotated, Any
                a: Annotated[Any, dynamic]
            "},
        );
    }

    #[test]
    fn callable_param_and_return() {
        check(
            "from typing import Callable\nf: Callable[[dynamic], dynamic]\n",
            indoc! {"
                from typing import Any
                from typing import Callable
                f: Callable[[Any], Any]
            "},
        );
    }

    #[test]
    fn cast_first_arg() {
        check(
            "from typing import cast\nb = cast(dynamic, a)\n",
            indoc! {"
                from typing import Any
                from typing import cast
                b = cast(Any, a)
            "},
        );
    }

    #[test]
    fn value_position_type_application() {
        check(
            "reveal_type(list[dynamic])\n",
            indoc! {"
                from typing import Any
                reveal_type(list[Any])
            "},
        );
    }

    #[test]
    fn class_base() {
        check(
            "class C(list[dynamic]): ...\n",
            indoc! {"
                from typing import Any
                class C(list[Any]): ...
            "},
        );
    }

    #[test]
    fn type_alias_rhs_py312() {
        // PY312+ keeps `type X = …` native; on PY310 the generics polyfill
        // would rewrite the whole statement (see `type_alias_rhs_py310`)
        check_py312(
            "type X = dynamic\n",
            indoc! {"
                from typing import Any
                type X = Any
            "},
        );
    }

    #[test]
    fn typeparam_bound_py312() {
        check_py312(
            "def f[T: dynamic](x: T) -> T: ...\n",
            indoc! {"
                from typing import Any
                def f[T: Any](x: T) -> T: ...
            "},
        );
    }

    #[test]
    fn type_alias_rhs_py310() {
        // the PEP-695 polyfill rewrites the whole alias on PY310; the dynamic
        // rewrite must be composed into that replacement so no bare `dynamic`
        // leaks into the output (it would be an undefined name, not a syntax
        // error, so the final parse can't catch it)
        let out = transpile("type X = dynamic\n", &Config::test_default()).unwrap();
        assert!(out.contains("Any"), "expected `Any` in output, got:\n{out}");
        assert!(
            !out.contains("dynamic"),
            "bare `dynamic` leaked into output:\n{out}"
        );
    }

    #[test]
    fn shadowed_not_rewritten() {
        // a local binding shadows the keyword — leave the annotation alone
        check(
            indoc! {"
                dynamic = int
                a: dynamic
            "},
            indoc! {"
                dynamic = int
                a: dynamic
            "},
        );
    }

    #[test]
    fn value_position_unchanged() {
        // `dynamic` outside a type position is an ordinary identifier
        unchanged("dynamic = 5\n");
        unchanged("print(dynamic)\n");
    }

    #[test]
    fn already_imported_any_no_duplicate() {
        // user already imports `Any` — don't prepend a second import line
        check(
            "from typing import Any\na: dynamic\n",
            indoc! {"
                from typing import Any
                a: Any
            "},
        );
    }
}
