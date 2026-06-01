//! lowers basedpython's special float-literal types `float.inf` / `float.nan`
//! (and `-float.inf`) to plain `float` in the transpiled output
//!
//! ty understands these forms in type positions as the infinity /
//! not-a-number float literals, but python has no literal syntax for them, so
//! the runtime artifact erases them to their `float` fallback. only the
//! unshadowed builtin `float` is rewritten — a local `float = …` keeps its
//! identity, matching ty's own resolution
//!
//! traversal is delegated to [`type_expr_walker`], so every type position the
//! walker recognises gets rewritten consistently with the other type-position
//! transforms. the walker visits the `float.inf` attribute node and the
//! `-float.inf` unary node directly (it does not descend into `-`), so both
//! shapes are matched here

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_one_type_expr, walk_type_positions,
};
use crate::type_info::TypeInfo;

pub(crate) struct FloatConst<'src> {
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> FloatConst<'src> {
    pub(crate) fn new(types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
        }
    }

    /// Run the erasure over a single type expression (used by the composed
    /// `rewrite_type_expr` in `just_float`, where a polyfill replaces the whole
    /// range and would otherwise subsume our in-place edits)
    pub(crate) fn emit_in_type_expr(&mut self, expr: &Expr) {
        walk_one_type_expr(expr, self);
    }

    /// is `expr` `float.inf` / `float.nan` on the unshadowed builtin `float`?
    fn is_float_constant(&self, expr: &Expr) -> bool {
        let Expr::Attribute(attr) = expr else {
            return false;
        };
        matches!(attr.attr.as_str(), "inf" | "nan")
            && matches!(attr.value.as_ref(), Expr::Name(base) if base.id.as_str() == "float")
            && self.types.is_unbound_at("float", attr.value.as_ref())
    }

    /// is `expr` a (possibly multiply) negated float constant — `-float.inf`,
    /// `--float.inf`, …? ty folds any unary-minus chain over a float literal, so
    /// the erasure must reach the same depth or the leftover `float.inf` crashes
    /// at runtime
    fn is_negated_float_constant(&self, expr: &Expr) -> bool {
        match expr {
            Expr::UnaryOp(u) if matches!(u.op, UnaryOp::USub) => {
                self.is_negated_float_constant(&u.operand)
            }
            other => self.is_float_constant(other),
        }
    }

    fn erase_to_float(&mut self, range: TextRange) {
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            "float".to_owned(),
            range,
        )));
    }
}

impl TypeExprVisitor for FloatConst<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // `float.inf` / `float.nan`
        if self.is_float_constant(expr) {
            self.erase_to_float(expr.range());
            return Recurse::Stop;
        }
        // `-float.inf` / `--float.inf` — the walker does not descend into `-`,
        // so match the whole negation chain and erase it in one edit
        if let Expr::UnaryOp(u) = expr
            && matches!(u.op, UnaryOp::USub)
            && self.is_negated_float_constant(&u.operand)
        {
            self.erase_to_float(expr.range());
            return Recurse::Stop;
        }
        Recurse::Descend
    }
}

pub(crate) struct FloatConstPass;

impl FloatConstPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for FloatConstPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = FloatConst::new(types);
        walk_type_positions(stmts, Some(types), &mut inner);
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
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn inf_annotation() {
        check("a: float.inf\n", "a: float\n");
    }

    #[test]
    fn nan_annotation() {
        check("a: float.nan\n", "a: float\n");
    }

    #[test]
    fn negative_inf_annotation() {
        check("a: -float.inf\n", "a: float\n");
    }

    #[test]
    fn double_negated_inf_annotation() {
        // ty folds any unary-minus chain over a float literal, so a deeper
        // negation must still erase rather than leak `float.inf` to runtime
        check("a: --float.inf\n", "a: float\n");
        check("a: ---float.nan\n", "a: float\n");
    }

    #[test]
    fn inf_in_union() {
        check("a: float.inf | None\n", "a: float | None\n");
    }

    #[test]
    fn inf_inside_generic() {
        check("a: list[float.inf]\n", "a: list[float]\n");
    }

    #[test]
    fn negative_inf_inside_generic() {
        check("a: list[-float.inf]\n", "a: list[float]\n");
    }

    #[test]
    fn inf_in_function_signature() {
        check(
            indoc! {"
                def f(x: float.inf) -> float.nan:
                    pass
            "},
            indoc! {"
                def f(x: float) -> float:
                    pass
            "},
        );
    }

    #[test]
    fn shadowed_float_not_rewritten() {
        // a local rebinding shadows the builtin — leave `float.inf` alone, in
        // agreement with ty (which won't treat it as a literal either)
        check(
            indoc! {"
                float = int
                a: float.inf
            "},
            indoc! {"
                float = int
                a: float.inf
            "},
        );
    }

    #[test]
    fn value_position_unchanged() {
        // `float.inf` outside a type position is an ordinary attribute access
        // (an `AttributeError` at runtime); the transform leaves it alone
        check("a = float.inf\n", "a = float.inf\n");
    }

    #[test]
    fn plain_float_attribute_unchanged() {
        // a different attribute is not a special float literal
        check("a: float.real\n", "a: float.real\n");
    }

    #[test]
    fn python_unchanged() {
        unchanged("a: float\n");
    }
}
