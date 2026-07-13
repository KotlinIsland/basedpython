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
//! - the value is undecidable statically (a mixed union, a dynamic value) but
//!   the target is a *user-defined* generic → probe the value's
//!   `__orig_class__`, natively stamped on user generics by `A[int](…)`;
//!   answers `False` for values that carry none. this soundly discriminates a
//!   `A[int] | A[str]` union too, per arm
//! - the value is undecidable and the target is a *builtin* collection, whose
//!   runtime instances erase their type arguments → ty errors
//!   (`erased-type-check`) and the lowering is the constant `False`. there is
//!   no element-witness heuristic: an empty `list[int]` has no element, and a
//!   builtin's element type is erased, so "check the first item" is unsound
//!
//! a subscripted rhs that is *not* a generic class (`x is candidates[0]`)
//! falls back to the ordinary `isinstance` lowering that
//! [`identity_swap`](super::identity_swap) applies to every other rhs

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, CmpOp, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::reified::is_keyword_comparison;
use ty_python_semantic::{ArgVariance, ParametricIsPlan};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// probes a value's `__orig_class__` against a target alias — the runtime
/// residue when a parametric test against a user-defined generic can't be
/// resolved statically. `variances` gives the target's per-parameter
/// variance (0 invariant, 1 covariant, 2 contravariant, 3 bivariant), so the
/// match respects `out T` / `in T`: `A[int]` is an `A[object]` when `T` is
/// covariant. `_sub` is a deliberately conservative one-level subtype check —
/// exact, the `object` top type, or an unparameterized supertype origin — so
/// it never reports a subtype that does not hold
pub(crate) const PARAMETRIC_IS_RUNTIME: &str = "\
def _parametric_is(value, alias, variances):
    origin = getattr(alias, \"__origin__\", alias)
    if not isinstance(value, origin):
        return False
    reified = getattr(value, \"__orig_class__\", None)
    if reified is None or getattr(reified, \"__origin__\", None) is not origin:
        return False
    reified_args = getattr(reified, \"__args__\", ())
    target_args = getattr(alias, \"__args__\", ())
    if len(reified_args) != len(target_args) or len(target_args) != len(variances):
        return False
    for r, t, v in zip(reified_args, target_args, variances):
        if v == 3 or r == t:
            continue
        if v == 1 and _parametric_is_sub(r, t):
            continue
        if v == 2 and _parametric_is_sub(t, r):
            continue
        return False
    return True

def _parametric_is_sub(a, b):
    if a is b or b is object:
        return True
    a_origin = getattr(a, \"__origin__\", a)
    b_origin = getattr(b, \"__origin__\", b)
    if isinstance(a_origin, type) and isinstance(b_origin, type) and not getattr(b, \"__args__\", ()):
        try:
            return issubclass(a_origin, b_origin)
        except TypeError:
            return False
    return a == b
";

struct ParametricIs<'src, 'ti> {
    source: &'src str,
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
    needs_probe: bool,
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
            // a builtin-target probe can never be true (ty reports the
            // error); lower it to the constant it always is
            ParametricIsPlan::Fold(false) | ParametricIsPlan::ErasedTarget => {
                let value = if negate { "True" } else { "False" };
                self.with_lhs_effects(lhs, value.to_owned())
            }
            ParametricIsPlan::Fold(true) => {
                let value = if negate { "False" } else { "True" };
                self.with_lhs_effects(lhs, value.to_owned())
            }
            ParametricIsPlan::TokenEq(tokens) => {
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
            // a user-defined generic target carries `__orig_class__`; probe it,
            // matching each type argument by the target's declared variance
            ParametricIsPlan::Probe(variances) => {
                self.needs_probe = true;
                let codes = variances
                    .iter()
                    .map(|variance| match variance {
                        ArgVariance::Invariant => "0",
                        ArgVariance::Covariant => "1",
                        ArgVariance::Contravariant => "2",
                        ArgVariance::Bivariant => "3",
                    })
                    .collect::<Vec<_>>();
                // a one-element tuple needs its trailing comma
                let tuple = match codes.as_slice() {
                    [single] => format!("({single},)"),
                    _ => format!("({})", codes.join(", ")),
                };
                let call = format!(
                    "_parametric_is({}, {}, {tuple})",
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
}

impl<'src> ParametricIsPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for ParametricIsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ParametricIs {
            source: self.source,
            types,
            edits: Vec::new(),
            needs_probe: false,
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        // no version gate is needed here: the only lowering that spells a
        // *builtin* subscript at runtime is the reified-cell token equality
        // (`T == list[int]`), which is already restricted to 3.12+ by the
        // reified-generic requirement; a user-generic probe (`A[int]`) works
        // on any target
        if inner.needs_probe {
            ctx.required_imports.push(PARAMETRIC_IS_RUNTIME.to_owned());
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
    fn covariant_subtype_folds_true() {
        // `A[int]` is an `A[object]` when `T` is covariant (`out T`), so a
        // statically-`A[int]` value folds the test to True
        let out = out(indoc! {"
            class A[out T]:
                def __init__(self): ...
            def f(a: A[int]) -> bool:
                return a is A[object]
        "});
        assert!(
            out.contains("return True"),
            "covariant A[int] is an A[object]: {out}"
        );
    }

    #[test]
    fn covariant_dynamic_probes_with_variance() {
        // a dynamic value against a covariant target emits a probe whose
        // variance code (1) makes the runtime match respect `out T`
        let out = out(indoc! {"
            class A[out T]:
                def __init__(self): ...
            def f(a: object) -> bool:
                return a is A[object]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[object], (1,))"),
            "covariant probe carries variance code 1: {out}"
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
    fn dynamic_value_against_user_generic_probes_orig_class() {
        // a user-defined generic's instances carry `__orig_class__`, so a
        // dynamic value against it is a valid runtime probe
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, t: T): ...
            def f(x) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, A[int], ("),
            "dynamic lhs against a user generic probes __orig_class__: {out}"
        );
        assert!(
            out.contains("def _parametric_is(value, alias, variances):"),
            "probe polyfill emitted: {out}"
        );
    }

    #[test]
    fn dynamic_value_against_builtin_folds_false() {
        // a builtin collection erases its type arguments at runtime, so the
        // probe can never succeed — ty errors and the lowering is a constant
        let out = out(indoc! {"
            def f(x) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return False"),
            "erased builtin target lowers to False: {out}"
        );
        assert!(
            !out.contains("_parametric_is"),
            "no probe against an erased builtin: {out}"
        );
    }

    #[test]
    fn builtin_union_is_an_error_not_a_witness() {
        // a builtin collection's element type is erased at runtime and an
        // empty list has no element to witness, so a builtin union can't be
        // discriminated — ty errors and the lowering is the constant `False`
        let out = out(indoc! {"
            def f(x: list[int] | list[str]) -> bool:
                return x is list[int]
        "});
        assert!(
            out.contains("return False"),
            "builtin union is erased, not witnessed: {out}"
        );
        assert!(
            !out.contains("_witness_is") && !out.contains("_parametric_is"),
            "no runtime heuristic for a builtin union: {out}"
        );
    }

    #[test]
    fn user_generic_union_probes_orig_class() {
        // a user generic carries `__orig_class__`, so a union against it is
        // soundly discriminated per arm by the probe (an invariant field
        // keeps ty from collapsing the union)
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, t: T):
                    self.v: list[T] = [t]
            def f(x: A[int] | A[str]) -> bool:
                return x is A[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, A[int], (0,))"),
            "user-generic union probes __orig_class__ with invariant T: {out}"
        );
    }

    #[test]
    fn union_excluding_target_folds_false() {
        // no arm matches `list[bytes]`, so the whole test folds statically,
        // independent of runtime erasure
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
    fn erased_builtin_probe_becomes_false() {
        // a dynamic value against a builtin target can't probe (no
        // __orig_class__); lowers to a constant, no polyfill
        let out = out(indoc! {"
            def f(x) -> bool:
                return x is dict[str, int]
        "});
        assert!(out.contains("return False"), "erased dict target: {out}");
        assert!(!out.contains("_parametric_is"), "no probe: {out}");
    }
}
