//! Rewrites bare `float` / `complex` in type-expression position to
//! `JustFloat` / `JustComplex` from `ty_extensions`.
//!
//! python's typing spec special-cases `float` to mean `int | float` and
//! `complex` to mean `int | float | complex`. basedpython does not — `float`
//! is just `float`, `complex` is just `complex`. the transpiler restores
//! python semantics by rewriting these names in type positions to the
//! corresponding `Just*` aliases, which expand to `TypeOf[1.0]` / `TypeOf[1.0j]`
//! and so escape the special case.
//!
//! emits a *minimal* edit per occurrence (just the name range), so it composes
//! with overlapping rewrites from `literal_types`, `intersection`, etc. only
//! the unshadowed builtin is rewritten — local `float = …` keeps its identity.
//!
//! traversal is delegated to [`type_expr_walker`], so every type position the
//! walker recognises (annotations, returns, type-alias RHS, type-param
//! bound/default, class bases, value-position type applications, `cast` first
//! arg, `Annotated` first arg, `Callable[[P], R]` param + return) gets
//! rewritten consistently with other type-position transforms

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, ExprName, Stmt};
use ruff_text_size::Ranged;

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::literal_types::LiteralType;
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_one_type_expr, walk_type_positions,
};
use crate::type_info::TypeInfo;

pub(crate) struct JustFloat<'src> {
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
    pub(crate) needs_float_alias: bool,
    pub(crate) needs_complex_alias: bool,
}

impl<'src> JustFloat<'src> {
    pub(crate) fn new(types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
            needs_float_alias: false,
            needs_complex_alias: false,
        }
    }

    /// is this name node the unshadowed builtin `float` / `complex`?
    fn rewrite_target(&self, name: &ExprName) -> Option<&'static str> {
        let id = name.id.as_str();
        let (target, replacement) = match id {
            "float" => ("float", "JustFloat"),
            "complex" => ("complex", "JustComplex"),
            _ => return None,
        };
        let anchor = Expr::Name(name.clone());
        if self.types.is_unbound_at(target, &anchor) {
            Some(replacement)
        } else {
            None
        }
    }

    /// public so [`rewrite_type_expr`] (used by `generics.rs`) can drive a
    /// one-off lowering over a single expression without spinning up a pass
    pub(crate) fn emit_in_type_expr(&mut self, expr: &Expr) {
        walk_one_type_expr(expr, self);
    }
}

impl TypeExprVisitor for JustFloat<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::Name(n) = expr
            && let Some(replacement) = self.rewrite_target(n)
        {
            if replacement == "JustFloat" {
                self.needs_float_alias = true;
            } else {
                self.needs_complex_alias = true;
            }
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                replacement.to_owned(),
                n.range(),
            )));
        }
        Recurse::Descend
    }
}

pub(crate) struct JustFloatPass;

impl JustFloatPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for JustFloatPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = JustFloat::new(types);
        walk_type_positions(stmts, Some(types), &mut inner);
        if inner.needs_float_alias {
            ctx.required_imports
                .push("from ty_extensions import JustFloat".to_owned());
        }
        if inner.needs_complex_alias {
            ctx.required_imports
                .push("from ty_extensions import JustComplex".to_owned());
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

/// Rewrite a type expression by composing the `literal_types` and
/// `just_float` lowerings into one. Returns `Some(text)` if either rewrite
/// fired, else `None` (caller should use the original source slice).
///
/// Used by `generics.rs` when the `type X = …` polyfill replaces the whole
/// alias range — its outer edit would otherwise subsume our minimal in-place
/// edits, so we have to splice the rewrite into the synthesized
/// `TypeAliasType("X", …)` payload directly.
pub(crate) fn rewrite_type_expr(source: &str, types: &dyn TypeInfo, expr: &Expr) -> Option<String> {
    let mut all_edits: Vec<Edit> = Vec::new();

    let mut lt = LiteralType::new(source, types);
    lt.emit_type_edits(expr, true);
    for fix in lt.edits {
        all_edits.extend(fix.into_edits());
    }

    let mut jf = JustFloat::new(types);
    jf.emit_in_type_expr(expr);
    for fix in jf.edits {
        all_edits.extend(fix.into_edits());
    }

    // `dynamic` → `Any` in the same composed rewrite, so a `type X = dynamic`
    // / `def f[T: dynamic]` polyfilled on Python < 3.12 doesn't leak the bare
    // keyword (an undefined name the final parse can't catch)
    let mut dk = crate::transforms::dynamic_keyword::DynamicKeyword::new(types);
    dk.emit_in_type_expr(expr);
    for fix in dk.edits {
        all_edits.extend(fix.into_edits());
    }

    // `float.inf` / `-float.inf` / `float.nan` → `float`, likewise — a
    // `def f[T: float.inf]` polyfilled to `TypeVar("_T", bound=…)` would
    // otherwise copy the bound verbatim and `AttributeError` at runtime
    let mut fc = crate::transforms::float_const::FloatConst::new(types);
    fc.emit_in_type_expr(expr);
    for fix in fc.edits {
        all_edits.extend(fix.into_edits());
    }

    if all_edits.is_empty() {
        return None;
    }
    all_edits.sort_by_key(Edit::start);

    let range = expr.range();
    let mut result = String::new();
    let mut pos = range.start();
    for edit in &all_edits {
        if edit.start() < pos {
            // overlap (shouldn't happen — literal_types and just_float touch
            // disjoint AST positions) — skip the conflicting edit
            continue;
        }
        result.push_str(&source[usize::from(pos)..usize::from(edit.start())]);
        result.push_str(edit.content().unwrap_or_default());
        pos = edit.end();
    }
    result.push_str(&source[usize::from(pos)..usize::from(range.end())]);
    Some(result)
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
    fn bare_float_annotation() {
        check(
            "a: float = 1.0\n",
            indoc! {"
                from ty_extensions import JustFloat
                a: JustFloat = 1.0
            "},
        );
    }

    #[test]
    fn bare_complex_annotation() {
        check(
            "a: complex = 1.0j\n",
            indoc! {"
                from ty_extensions import JustComplex
                a: JustComplex = 1.0j
            "},
        );
    }

    #[test]
    fn float_and_complex_in_same_module() {
        check(
            indoc! {"
                a: float
                b: complex
            "},
            indoc! {"
                from ty_extensions import JustComplex, JustFloat
                a: JustFloat
                b: JustComplex
            "},
        );
    }

    #[test]
    fn float_in_union() {
        check(
            "a: float | int\n",
            indoc! {"
                from ty_extensions import JustFloat
                a: JustFloat | int
            "},
        );
    }

    #[test]
    fn float_inside_list_generic() {
        check(
            "a: list[float]\n",
            indoc! {"
                from ty_extensions import JustFloat
                a: list[JustFloat]
            "},
        );
    }

    #[test]
    fn float_inside_dict_generic() {
        check(
            "a: dict[str, float]\n",
            indoc! {"
                from ty_extensions import JustFloat
                a: dict[str, JustFloat]
            "},
        );
    }

    #[test]
    fn function_param_and_return() {
        check(
            indoc! {"
                def f(x: float) -> complex:
                    pass
            "},
            indoc! {"
                from ty_extensions import JustComplex, JustFloat
                def f(x: JustFloat) -> JustComplex:
                    pass
            "},
        );
    }

    #[test]
    fn annotated_first_arg_only() {
        check(
            "a: Annotated[float, \"meta with float word\"]\n",
            indoc! {"
                from ty_extensions import JustFloat
                from typing import Annotated
                a: Annotated[JustFloat, \"meta with float word\"]
            "},
        );
    }

    #[test]
    fn literal_slice_not_rewritten() {
        // `Literal[…]` slice is a value position — `"float"` here is a
        // string value, not a type expression, so leave it alone
        check("a: Literal[\"float\"]\n", "a: Literal[\"float\"]\n");
    }

    #[test]
    fn shadowed_float_not_rewritten() {
        // local rebinding shadows the builtin — leave the annotation alone
        check(
            indoc! {"
                float = int
                a: float
            "},
            indoc! {"
                float = int
                a: float
            "},
        );
    }

    #[test]
    fn type_alias_rhs_312() {
        check_py312(
            "type X = float | int\n",
            indoc! {"
                from ty_extensions import JustFloat
                type X = JustFloat | int
            "},
        );
    }

    #[test]
    fn typevar_bound() {
        check_py312(
            "def f[T: float](x: T) -> T: return x\n",
            indoc! {"
                from ty_extensions import JustFloat
                def f[T: JustFloat](x: T) -> T: return x
            "},
        );
    }

    #[test]
    fn value_context_unchanged() {
        // `float(x)` is a call, not a type expression
        check("a = float(1)\n", "a = float(1)\n");
    }

    #[test]
    fn isinstance_arg_unchanged() {
        // `isinstance(x, float)` — the second arg is a value (the class
        // object), not a type-expression position. don't rewrite
        check(
            "def f(x):\n    return isinstance(x, float)\n",
            "def f(x):\n    return isinstance(x, float)\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("a: float\n");
        unchanged("a: complex\n");
    }

    #[test]
    fn float_in_class_base() {
        check(
            "class C(list[float]): ...\n",
            indoc! {"
                from ty_extensions import JustFloat
                class C(list[JustFloat]): ...
            "},
        );
    }

    #[test]
    fn float_in_value_position_type_application() {
        check(
            "reveal_type(list[float])\n",
            indoc! {"
                from ty_extensions import JustFloat
                reveal_type(list[JustFloat])
            "},
        );
    }

    #[test]
    fn float_in_cast_first_arg() {
        check(
            "from typing import cast\nb = cast(float, a)\n",
            indoc! {"
                from ty_extensions import JustFloat
                from typing import cast
                b = cast(JustFloat, a)
            "},
        );
    }

    #[test]
    fn float_in_callable_param_and_return() {
        check(
            "from typing import Callable\nf: Callable[[float], complex]\n",
            indoc! {"
                from ty_extensions import JustComplex, JustFloat
                from typing import Callable
                f: Callable[[JustFloat], JustComplex]
            "},
        );
    }
}
