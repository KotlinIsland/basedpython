//! parametric type tests (basedpython)
//!
//! `x is list[int]` (keyword form) tests a value against a *specialization*,
//! which plain `isinstance` cannot do — `isinstance(x, list[int])` is a
//! runtime `TypeError`, and the builtins erase their type arguments anyway.
//! the test is resolved rust-style, from static types at compile time, with
//! a runtime residue only where one is needed:
//!
//! - the value's type is fully known → the test folds to `True` / `False`
//!   (invariant, exact arguments — `list[object]` is never `list[int]`)
//! - the value's type mentions reified type parameters → the test unifies
//!   structurally and lowers to equality checks of the reified cells:
//!   `x: T` is `list[int]` → `T == list[int]`; `x: list[T]` is `list[int]`
//!   → `T == int`
//! - the value is a union of same-origin specializations with disjoint
//!   arguments → one witness element decides the arm (`x: list[int] |
//!   list[str]` is `list[int]` → probe the first element; an empty
//!   collection has no witness and answers `False`)
//! - anything else is *unchecked* (ty warns): the lowering probes the
//!   value's `__orig_class__` — natively stamped on user generics by
//!   `A[int](…)` — and answers `False` for values that carry none
//!
//! a subscripted rhs that is *not* a generic class (`x is candidates[0]`)
//! falls back to the ordinary `isinstance` lowering that
//! [`identity_swap`](super::identity_swap) applies to every other rhs

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, CmpOp, Expr, PythonVersion, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::reified::is_keyword_comparison;
use ty_python_semantic::{ParametricIsPlan, WitnessPlan};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// probes a value's `__orig_class__` against a target alias — the unchecked
/// fallback when a parametric test can't be resolved statically
pub(crate) const PARAMETRIC_IS_RUNTIME: &str = "\
def _parametric_is(value, alias):
    origin = getattr(alias, \"__origin__\", alias)
    if not isinstance(value, origin):
        return False
    reified = getattr(value, \"__orig_class__\", None)
    return (
        reified is not None
        and getattr(reified, \"__origin__\", None) is origin
        and getattr(reified, \"__args__\", None) == alias.__args__
    )
";

/// whether a collection's first element is an instance of `ty` — decides
/// which arm of a disjoint union a value is. empty means no witness: `False`
pub(crate) const WITNESS_RUNTIME: &str = "\
def _witness_is(items, ty):
    for item in items:
        return isinstance(item, ty)
    return False
";

struct ParametricIs<'src, 'ti> {
    source: &'src str,
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
    needs_probe: bool,
    needs_witness: bool,
    /// a lowering evaluates the subscripted alias at runtime (token equality
    /// or probe), which needs pep 585 for the builtins
    needs_runtime_alias: bool,
}

impl ParametricIs<'_, '_> {
    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// whether replacing the pair may drop the lhs without losing effects
    fn effect_free(lhs: &Expr) -> bool {
        matches!(lhs, Expr::Name(_)) || lhs.is_literal_expr()
    }

    /// the folded / token result expression, with the lhs kept alive when it
    /// may have effects (`(g(), True)[1]` evaluates then discards it)
    fn with_lhs_effects(&self, lhs: &Expr, result: String) -> String {
        if Self::effect_free(lhs) {
            result
        } else {
            format!("({}, {result})[1]", self.src(lhs.range()))
        }
    }

    fn lower_pair(&mut self, lhs: &Expr, rhs: &ast::ExprSubscript, negate: bool) -> String {
        let Some(plan) = self.types.parametric_is_plan(lhs, rhs) else {
            // not a generic-class rhs — the ordinary isinstance lowering
            // identity_swap applies everywhere else
            let call = format!(
                "isinstance({}, {})",
                self.src(lhs.range()),
                self.src(rhs.range())
            );
            return if negate { format!("not {call}") } else { call };
        };
        match plan {
            ParametricIsPlan::Fold(result) => {
                let value = if result != negate { "True" } else { "False" };
                self.with_lhs_effects(lhs, value.to_owned())
            }
            ParametricIsPlan::TokenEq(tokens) => {
                self.needs_runtime_alias = true;
                let comparisons = tokens
                    .iter()
                    .map(|(name, target)| format!("{name} == {}", self.src(*target)))
                    .collect::<Vec<_>>()
                    .join(" and ");
                let check = if negate {
                    format!("not ({comparisons})")
                } else {
                    format!("({comparisons})")
                };
                self.with_lhs_effects(lhs, check)
            }
            ParametricIsPlan::Witness(witness) => {
                self.needs_witness = true;
                let lhs_src = self.src(lhs.range());
                let check = match witness {
                    WitnessPlan::Element { class } => {
                        format!("_witness_is({lhs_src}, {})", self.src(class))
                    }
                    WitnessPlan::DictKey { class } => {
                        format!("_witness_is(({lhs_src}).keys(), {})", self.src(class))
                    }
                    WitnessPlan::DictValue { class } => {
                        format!("_witness_is(({lhs_src}).values(), {})", self.src(class))
                    }
                    WitnessPlan::TupleIndex { index, class } => {
                        format!("isinstance(({lhs_src})[{index}], {})", self.src(class))
                    }
                };
                if negate {
                    format!("not {check}")
                } else {
                    check
                }
            }
            ParametricIsPlan::Probe(_) => {
                self.needs_probe = true;
                self.needs_runtime_alias = true;
                let call = format!(
                    "_parametric_is({}, {})",
                    self.src(lhs.range()),
                    self.src(rhs.range())
                );
                if negate { format!("not {call}") } else { call }
            }
        }
    }

    fn process_compare(&mut self, compare: &ast::ExprCompare) {
        let mut lhs: &Expr = &compare.left;
        for (op, rhs) in compare.ops.iter().zip(&compare.comparators) {
            if matches!(op, CmpOp::Is | CmpOp::IsNot)
                && let Expr::Subscript(subscript) = rhs
                && is_keyword_comparison(self.source, *op, lhs, rhs)
            {
                let replacement = self.lower_pair(lhs, subscript, matches!(op, CmpOp::IsNot));
                let pair_range = TextRange::new(lhs.range().start(), rhs.range().end());
                self.edits.push((pair_range, replacement));
            }
            lhs = rhs;
        }
    }
}

impl<'ast> Visitor<'ast> for ParametricIs<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Compare(compare) = expr {
            self.process_compare(compare);
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct ParametricIsPass<'src> {
    source: &'src str,
    min_version: PythonVersion,
}

impl<'src> ParametricIsPass<'src> {
    pub(crate) fn new(source: &'src str, min_version: PythonVersion) -> Self {
        Self {
            source,
            min_version,
        }
    }
}

impl TypeAwarePass for ParametricIsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ParametricIs {
            source: self.source,
            types,
            edits: Vec::new(),
            needs_probe: false,
            needs_witness: false,
            needs_runtime_alias: false,
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        // folds and witness probes reference only plain classes; token and
        // probe lowerings evaluate the subscripted alias itself, and pep 585
        // (3.9) is what makes the builtins subscriptable there
        if self.min_version < PythonVersion::PY39 && inner.needs_runtime_alias {
            ctx.errors.push(
                "parametric type tests (`x is list[int]`) require python 3.9 or newer: \
                 the lowered check subscripts the target class at runtime"
                    .to_owned(),
            );
            return;
        }
        if inner.needs_probe {
            ctx.required_imports.push(PARAMETRIC_IS_RUNTIME.to_owned());
        }
        if inner.needs_witness {
            ctx.required_imports.push(WITNESS_RUNTIME.to_owned());
        }
        ctx.text_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn out(input: &str) -> String {
        transpile(
            input,
            &Config {
                min_version: PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn concrete_mismatch_folds_false() {
        let out = out(indoc! {"
            xs: list[object] = [1]
            print(xs is list[int])
        "});
        assert!(out.contains("print(False)"), "should fold false: {out}");
    }

    #[test]
    fn concrete_match_folds_true() {
        let out = out(indoc! {"
            xs = [1, 2]
            print(xs is list[int])
        "});
        assert!(out.contains("print(True)"), "should fold true: {out}");
    }

    #[test]
    fn disjoint_class_folds_false() {
        let out = out(indoc! {"
            x = \"s\"
            print(x is list[int])
        "});
        assert!(
            out.contains("print(False)"),
            "str excludes list[int]: {out}"
        );
    }

    #[test]
    fn effectful_lhs_preserved_in_fold() {
        let out = out(indoc! {"
            def g() -> list[int]:
                return [1]
            print(g() is list[int])
        "});
        assert!(
            out.contains("print((g(), True)[1])"),
            "side effects must survive the fold: {out}"
        );
    }

    #[test]
    fn bare_typevar_compares_reified_cell() {
        let out = out(indoc! {"
            def f[T](x: T) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return (T == list[int])"),
            "bare typevar compares whole alias: {out}"
        );
        assert!(
            out.contains("@generic  # basedpython: reified"),
            "the parametric test must reify T: {out}"
        );
    }

    #[test]
    fn structural_typevar_unifies() {
        let out = out(indoc! {"
            def f[T](x: list[T]) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return (T == int)"),
            "list[T] vs list[int] unifies to T == int: {out}"
        );
    }

    #[test]
    fn is_not_negates() {
        let out = out(indoc! {"
            def f[T](x: T) -> bool:
                return x is not list[int]
        "});
        assert!(
            out.contains("return not (T == list[int])"),
            "is not negates the token check: {out}"
        );
    }

    #[test]
    fn dynamic_value_probes_orig_class() {
        let out = out(indoc! {"
            def f(x) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, list[int])"),
            "dynamic lhs lowers to the probe: {out}"
        );
        assert!(
            out.contains("def _parametric_is(value, alias):"),
            "probe polyfill emitted: {out}"
        );
    }

    #[test]
    fn disjoint_union_uses_witness() {
        let out = out(indoc! {"
            def f(x: list[int] | list[str]) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return _witness_is(x, int)"),
            "disjoint union discriminates by witness: {out}"
        );
        assert!(
            out.contains("def _witness_is(items, ty):"),
            "witness polyfill emitted: {out}"
        );
    }

    #[test]
    fn dict_union_witnesses_discriminating_side() {
        let out = out(indoc! {"
            def f(x: dict[str, int] | dict[str, bytes]) -> bool:
                return x is dict[str, int]
        "});
        assert!(
            out.contains("return _witness_is((x).values(), int)"),
            "values discriminate when keys agree: {out}"
        );
    }

    #[test]
    fn union_excluding_target_folds_false() {
        let out = out(indoc! {"
            def f(x: list[int] | list[str]) -> bool:
                return x is list[bytes]
        "});
        assert!(
            out.contains("return False"),
            "target outside the union folds false: {out}"
        );
    }

    #[test]
    fn value_subscript_rhs_falls_back_to_isinstance() {
        let out = out(indoc! {"
            class A: ...
            pair = (A, A)
            x = A()
            print(x is pair[0])
        "});
        assert!(
            out.contains("print(isinstance(x, pair[0]))"),
            "non-alias subscript rhs is a plain isinstance: {out}"
        );
    }

    #[test]
    fn identity_operator_untouched() {
        // `===` keeps python identity semantics even against an alias
        let out = out(indoc! {"
            xs = [1]
            print(xs === list[int])
        "});
        assert!(
            out.contains("print(xs is list[int])"),
            "=== stays identity: {out}"
        );
    }

    #[test]
    fn below_39_is_an_error() {
        let err = transpile(
            indoc! {"
                def f(x) -> bool:
                    return x is list[int]
            "},
            &Config {
                min_version: PythonVersion::PY38,
                ..Config::test_default()
            },
        )
        .unwrap_err();
        assert!(
            err.contains("3.9"),
            "expected a 3.9 requirement error: {err}"
        );
    }
}
