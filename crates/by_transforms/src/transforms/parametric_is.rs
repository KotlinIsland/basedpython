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
//! a union rhs (`x is A[int] | object`) is the disjunction of its arms — each
//! arm lowered by its own kind — never a runtime `isinstance(x, A[int] |
//! object)`, which a parameterized arm makes a `TypeError`
//!
//! a subscripted rhs that is *not* a generic class (`x is candidates[0]`)
//! falls back to the ordinary `isinstance` lowering that
//! [`identity_swap`](super::identity_swap) applies to every other rhs

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, CmpOp, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::reified::is_keyword_comparison;
use ty_python_semantic::{ArgVariance, ParametricIsPlan};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// probes a value's `__orig_class__` against a target alias — the runtime
/// residue when a parametric test against a user-defined generic can't be
/// resolved statically. `variances` gives the target's per-parameter
/// variance (0 invariant, 1 covariant, 2 contravariant, 3 bivariant), so the
/// match respects `out T` / `in T`: `A[int]` is an `A[object]` when `T` is
/// covariant. `_sub` is a deliberately conservative one-level subtype check —
/// exact, the `object` top type, or an unparameterized supertype origin — so
/// it never reports a subtype that does not hold
/// render a variance-code list as the python tuple literal `_parametric_is`
/// takes as its `variances` argument (`(0,)`, `(0, 1)`)
pub(crate) fn variance_tuple(variances: &[u8]) -> String {
    match variances {
        [single] => format!("({single},)"),
        _ => format!(
            "({})",
            variances
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) const PARAMETRIC_IS_RUNTIME: &str = "\
def _parametric_is(value, alias, variances):
    alias = getattr(alias, \"__value__\", alias)
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
    edits: Vec<(TextRange, Vec<Fragment>)>,
    needs_probe: bool,
}

impl ParametricIs<'_, '_> {
    /// whether replacing the pair may drop the lhs without losing effects
    fn effect_free(lhs: &Expr) -> bool {
        matches!(lhs, Expr::Name(_)) || lhs.is_literal_expr()
    }

    /// the folded / token result expression, with the lhs kept alive when it
    /// may have effects (`(g(), True)[1]` evaluates then discards it)
    fn with_lhs_effects(lhs: &Expr, result: Vec<Fragment>) -> Vec<Fragment> {
        if Self::effect_free(lhs) {
            return result;
        }
        let mut frags = vec![Fragment::Lit("(".to_owned()), Fragment::Src(lhs.range())];
        frags.push(Fragment::Lit(", ".to_owned()));
        frags.extend(result);
        frags.push(Fragment::Lit(")[1]".to_owned()));
        frags
    }

    fn lower_pair(&mut self, lhs: &Expr, rhs: &Expr, negate: bool) -> Vec<Fragment> {
        let Some(plan) = self.types.parametric_is_plan(lhs, rhs) else {
            // rhs is a bare class or a plain value, not a specialization — the
            // ordinary isinstance lowering that identity_swap used to emit
            let open = if negate {
                "not isinstance("
            } else {
                "isinstance("
            };
            return vec![
                Fragment::Lit(open.to_owned()),
                Fragment::Src(lhs.range()),
                Fragment::Lit(", ".to_owned()),
                Fragment::Src(rhs.range()),
                Fragment::Lit(")".to_owned()),
            ];
        };
        match plan {
            // an erased-target probe (builtin or protocol) can never be true
            // (ty reports the error); lower it to the constant it always is
            ParametricIsPlan::Fold(false) | ParametricIsPlan::ErasedTarget(_) => {
                let value = if negate { "True" } else { "False" };
                Self::with_lhs_effects(lhs, vec![Fragment::Lit(value.to_owned())])
            }
            ParametricIsPlan::Fold(true) => {
                let value = if negate { "False" } else { "True" };
                Self::with_lhs_effects(lhs, vec![Fragment::Lit(value.to_owned())])
            }
            ParametricIsPlan::TokenEq(tokens) => {
                let mut frags = vec![Fragment::Lit(if negate { "not (" } else { "(" }.to_owned())];
                for (index, (name, target)) in tokens.iter().enumerate() {
                    let lead = if index == 0 { "" } else { " and " };
                    frags.push(Fragment::Lit(format!("{lead}{name} == ")));
                    frags.push(Fragment::Src(*target));
                }
                frags.push(Fragment::Lit(")".to_owned()));
                Self::with_lhs_effects(lhs, frags)
            }
            // a user-defined generic target carries `__orig_class__`; probe it,
            // matching each type argument by the target's effective variance
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
                let open = if negate {
                    "not _parametric_is("
                } else {
                    "_parametric_is("
                };
                vec![
                    Fragment::Lit(open.to_owned()),
                    Fragment::Src(lhs.range()),
                    Fragment::Lit(", ".to_owned()),
                    Fragment::Src(rhs.range()),
                    Fragment::Lit(format!(", {tuple})")),
                ]
            }
        }
    }

    /// one arm of a union `is`-target: its bare (non-negated) test, referencing
    /// the value through `value` rather than the lhs directly so the caller can
    /// bind the lhs once and share it across arms. no lhs-effect wrapping here —
    /// the union combiner evaluates the lhs exactly once for the whole test
    fn lower_arm(&mut self, value: &dyn Fn() -> Fragment, lhs: &Expr, arm: &Expr) -> Vec<Fragment> {
        // a `None` arm (an `X | None` optional) is an identity check, not
        // `isinstance(_, None)` — `None` is a value, not a class
        if matches!(arm, Expr::NoneLiteral(_)) {
            return vec![value(), Fragment::Lit(" is None".to_owned())];
        }
        let Some(plan) = self.types.parametric_is_plan(lhs, arm) else {
            return vec![
                Fragment::Lit("isinstance(".to_owned()),
                value(),
                Fragment::Lit(", ".to_owned()),
                Fragment::Src(arm.range()),
                Fragment::Lit(")".to_owned()),
            ];
        };
        match plan {
            // an erased arm can't be checked at runtime; ty reports the error
            // (a union arm may not silently fold to `False` — that would be
            // unsound — so the checker rejects it), and the lowering is the
            // constant the standalone form uses
            ParametricIsPlan::Fold(false) | ParametricIsPlan::ErasedTarget(_) => {
                vec![Fragment::Lit("False".to_owned())]
            }
            ParametricIsPlan::Fold(true) => vec![Fragment::Lit("True".to_owned())],
            ParametricIsPlan::TokenEq(tokens) => {
                let mut frags = vec![Fragment::Lit("(".to_owned())];
                for (index, (name, target)) in tokens.iter().enumerate() {
                    let lead = if index == 0 { "" } else { " and " };
                    frags.push(Fragment::Lit(format!("{lead}{name} == ")));
                    frags.push(Fragment::Src(*target));
                }
                frags.push(Fragment::Lit(")".to_owned()));
                frags
            }
            ParametricIsPlan::Probe(variances) => {
                self.needs_probe = true;
                let codes: Vec<u8> = variances
                    .iter()
                    .map(|variance| match variance {
                        ArgVariance::Invariant => 0,
                        ArgVariance::Covariant => 1,
                        ArgVariance::Contravariant => 2,
                        ArgVariance::Bivariant => 3,
                    })
                    .collect();
                vec![
                    Fragment::Lit("_parametric_is(".to_owned()),
                    value(),
                    Fragment::Lit(", ".to_owned()),
                    Fragment::Src(arm.range()),
                    Fragment::Lit(format!(", {})", variance_tuple(&codes))),
                ]
            }
        }
    }

    /// `lhs is (T1 | T2 | …)` — a test against a union type — is the disjunction
    /// of the per-arm tests (`type(lhs) <: Ti` for any arm). the lhs is bound
    /// once: referenced directly when it has no effects, else through a lambda
    /// parameter so the arms share a single evaluation
    fn lower_union(&mut self, lhs: &Expr, arms: &[&Expr], negate: bool) -> Vec<Fragment> {
        let via_lambda = !Self::effect_free(lhs);
        let value = || {
            if via_lambda {
                Fragment::Lit(UNION_VALUE_PARAM.to_owned())
            } else {
                Fragment::Src(lhs.range())
            }
        };
        let mut inner: Vec<Fragment> = Vec::new();
        for (index, arm) in arms.iter().enumerate() {
            if index > 0 {
                inner.push(Fragment::Lit(" or ".to_owned()));
            }
            inner.extend(self.lower_arm(&value, lhs, arm));
        }
        let mut frags = Vec::new();
        if via_lambda {
            frags.push(Fragment::Lit(format!(
                "{}(lambda {UNION_VALUE_PARAM}: ",
                if negate { "not " } else { "" }
            )));
            frags.extend(inner);
            frags.push(Fragment::Lit(")(".to_owned()));
            frags.push(Fragment::Src(lhs.range()));
            frags.push(Fragment::Lit(")".to_owned()));
        } else {
            frags.push(Fragment::Lit(if negate { "not (" } else { "(" }.to_owned()));
            frags.extend(inner);
            frags.push(Fragment::Lit(")".to_owned()));
        }
        frags
    }

    fn process_compare(&mut self, compare: &ast::ExprCompare) {
        let mut lhs: &Expr = &compare.left;
        for (op, rhs) in compare.ops.iter().zip(&compare.comparators) {
            // identity_swap defers every non-literal name/attribute/subscript
            // and union `is`-rhs to this pass, which owns the
            // isinstance-vs-parametric decision (a bare class → isinstance, a
            // specialization or an alias to one → a parametric test, a union →
            // the disjunction of the arms)
            if matches!(op, CmpOp::Is | CmpOp::IsNot)
                && is_keyword_comparison(self.source, *op, lhs, rhs)
                // a subscript that resolves to a plain value (`candidates[0]`
                // holding an enum member) keeps python identity semantics,
                // same as identity_swap's rule for unsubscripted rhs
                && !self.types.is_keeps_identity(rhs)
            {
                let negate = matches!(op, CmpOp::IsNot);
                let replacement =
                    if matches!(rhs, Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_)) {
                        Some(self.lower_pair(lhs, rhs, negate))
                    } else {
                        union_arms(rhs).map(|arms| self.lower_union(lhs, &arms, negate))
                    };
                if let Some(replacement) = replacement {
                    let pair_range = TextRange::new(lhs.range().start(), rhs.range().end());
                    self.edits.push((pair_range, replacement));
                }
            }
            lhs = rhs;
        }
    }
}

/// the lambda parameter that binds an effectful union-test lhs for its arms.
/// unlikely to collide: an `is`-target arm is a type expression, and this name
/// would have to appear free inside one
const UNION_VALUE_PARAM: &str = "_by_is_value";

/// the flat arms of a union type expression (`A | B | C` → `[A, B, C]`), or
/// `None` when `expr` is not a `|` union
fn union_arms(expr: &Expr) -> Option<Vec<&Expr>> {
    if !matches!(expr, Expr::BinOp(binop) if binop.op == ast::Operator::BitOr) {
        return None;
    }
    let mut arms = Vec::new();
    collect_union_arms(expr, &mut arms);
    Some(arms)
}

fn collect_union_arms<'a>(expr: &'a Expr, arms: &mut Vec<&'a Expr>) {
    if let Expr::BinOp(binop) = expr
        && binop.op == ast::Operator::BitOr
    {
        collect_union_arms(&binop.left, arms);
        collect_union_arms(&binop.right, arms);
    } else {
        arms.push(expr);
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
        ctx.template_edits.extend(inner.edits);
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
    fn use_site_covariant_target_probes_covariantly() {
        // `A[out int]` projects an invariant `T` covariantly for this one
        // test, so the probe matches with code 1 — and the target renders as
        // plain `A[int]`, the keyword having no runtime spelling
        let out = out(indoc! {"
            class A[in out T]:
                def __init__(self): ...
            def f(a: A[*]) -> bool:
                return a is A[out int]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[int], (1,))"),
            "use-site `out` probes covariantly: {out}"
        );
    }

    #[test]
    fn use_site_contravariant_target_probes_contravariantly() {
        let out = out(indoc! {"
            class S[in out T]:
                def __init__(self): ...
            def f(s: S[*]) -> bool:
                return s is S[in bool]
        "});
        assert!(
            out.contains("return _parametric_is(s, S[bool], (2,))"),
            "use-site `in` probes contravariantly: {out}"
        );
    }

    #[test]
    fn unprojected_invariant_target_probes_invariantly() {
        // the counterpart to the two above: without a projection an invariant
        // `T` keeps demanding an exact match
        let out = out(indoc! {"
            class A[in out T]:
                def __init__(self): ...
            def f(a: A[*]) -> bool:
                return a is A[int]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[int], (0,))"),
            "no projection stays invariant: {out}"
        );
    }

    #[test]
    fn use_site_covariant_target_folds_true() {
        // `A[bool]` is an `A[out int]` statically, so this folds rather than
        // probing — the fold must agree with assignability, not contradict it
        let out = out(indoc! {"
            class A[in out T]:
                def __init__(self): ...
            def f(a: A[bool]) -> bool:
                return a is A[out int]
        "});
        assert!(
            out.contains("return True"),
            "A[bool] is an A[out int]: {out}"
        );
    }

    #[test]
    fn use_site_variance_on_declared_covariant_target_is_a_no_op() {
        // a declared `out T` already covers what the projection could give
        let out = out(indoc! {"
            class A[out T]:
                def __init__(self): ...
            def f(a: object) -> bool:
                return a is A[out object]
        "});
        assert!(
            out.contains("return _parametric_is(a, A[object], (1,))"),
            "declared variance wins: {out}"
        );
    }

    #[test]
    fn reified_tuple_target_compares_each_cell() {
        // a `tuple[T, U]` value unifies against the tuple target position by
        // position — the `Tuple::Fixed` unify branch
        let out = out(indoc! {"
            def f[T, U](x: tuple[T, U]) -> bool:
                return x is tuple[int, str]
        "});
        assert!(
            out.contains("return (T == int and U == str)"),
            "tuple target compares each cell: {out}"
        );
    }

    #[test]
    fn nested_generic_value_unifies_recursively() {
        // `A[list[T]]` reaches `T` through two levels; the unify descends the
        // target structure to the cell
        let out = out(indoc! {"
            class A[T]:
                def __init__(self): ...
            def f[T](x: A[list[T]]) -> bool:
                return x is A[list[int]]
        "});
        assert!(
            out.contains("return (T == int)"),
            "nested value unifies to the inner cell: {out}"
        );
    }

    #[test]
    fn multi_param_probe_carries_a_variance_per_param() {
        // two invariant parameters → a two-entry variance tuple, exercising the
        // plural branch of the tuple spelling and the polyfill's per-arg loop
        let out = out(indoc! {"
            class Pair[K, V]:
                def __init__(self, k: K, v: V):
                    self.k: K = k
                    self.v: V = v
            def f(x: object) -> bool:
                return x is Pair[int, str]
        "});
        assert!(
            out.contains("return _parametric_is(x, Pair[int, str], (0, 0))"),
            "one variance code per parameter: {out}"
        );
    }

    #[test]
    fn bivariant_typevar_probes_with_code_three() {
        // a parameter unused in the class body is bivariant; the probe matches
        // either way (code 3)
        let out = out(indoc! {"
            class Box[T]:
                def __init__(self): ...
            def f(x: object) -> bool:
                return x is Box[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, Box[int], (3,))"),
            "bivariant parameter probes with code 3: {out}"
        );
    }

    #[test]
    fn declared_contravariant_dynamic_probes_with_code_two() {
        // the counterpart to `covariant_dynamic_probes_with_variance` for a
        // declared `in T` — the probe carries variance code 2
        let out = out(indoc! {"
            class Sink[in T]:
                def __init__(self): ...
                def put(self, x: T) -> None: ...
            def f(x: object) -> bool:
                return x is Sink[int]
        "});
        assert!(
            out.contains("return _parametric_is(x, Sink[int], (2,))"),
            "declared contravariant probes with code 2: {out}"
        );
    }

    #[test]
    fn protocol_target_probe_becomes_false() {
        // a protocol target can't be probed (its instances never carry a
        // matching `__orig_class__`); ty reports the error and the lowering is
        // the constant it always is, with no polyfill
        let out = out(indoc! {"
            from typing import Protocol
            class P[T](Protocol):
                def get(self) -> T: ...
            def f(x: object) -> bool:
                return x is P[int]
        "});
        assert!(out.contains("return False"), "protocol target folds: {out}");
        assert!(!out.contains("_parametric_is"), "no probe emitted: {out}");
    }

    #[test]
    fn implicit_alias_target_probes_like_the_specialization() {
        // `X = A[int]` binds `X` to the specialization itself, so `y is X`
        // resolves exactly as `y is A[int]` would — a probe here
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, v: T):
                    self.v: list[T] = [v]
            X = A[int]
            def f(y: object) -> bool:
                return y is X
        "});
        assert!(
            out.contains("return _parametric_is(y, X, (0,))"),
            "alias name probes through `X`: {out}"
        );
    }

    #[test]
    fn implicit_alias_builtin_target_is_erased() {
        // an alias to a builtin specialization is erased just like the direct
        // form — a constant, and ty reports the error
        let out = out(indoc! {"
            X = list[int]
            def f(y: object) -> bool:
                return y is X
        "});
        assert!(out.contains("return False"), "erased alias target: {out}");
        assert!(!out.contains("_parametric_is"), "no probe: {out}");
    }

    #[test]
    fn pep695_type_alias_target_probes_through_value() {
        // `type W = A[int]` is a `TypeAliasType`; the probe unwraps `.__value__`
        // at runtime, so `y is W` still resolves against `A[int]`
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, v: T):
                    self.v: list[T] = [v]
            type W = A[int]
            def f(y: object) -> bool:
                return y is W
        "});
        assert!(
            out.contains("return _parametric_is(y, W, (0,))"),
            "type alias probes through `W`: {out}"
        );
    }

    #[test]
    fn bare_class_name_still_lowers_to_isinstance() {
        // a non-generic name target is not a specialization; it keeps the
        // ordinary isinstance lowering that identity_swap used to own
        let out = out(indoc! {"
            def f(y: object) -> bool:
                return y is int
        "});
        assert!(
            out.contains("return isinstance(y, int)"),
            "bare class → isinstance: {out}"
        );
    }

    #[test]
    fn union_of_plain_classes_ors_isinstance() {
        // a union target is the disjunction of its arms — never a runtime
        // `isinstance(a, X | Y)`, which fails on a parameterized arm and even on
        // plain classes before python 3.10
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is int | str
        "});
        assert!(
            out.contains("return (isinstance(a, int) or isinstance(a, str))"),
            "plain union ORs isinstance per arm: {out}"
        );
    }

    #[test]
    fn union_mixes_probe_and_isinstance_per_arm() {
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, v: T):
                    self.v: list[T] = [v]
            def f(a: object) -> bool:
                return a is A[int] | object
        "});
        assert!(
            out.contains("return (_parametric_is(a, A[int], (0,)) or isinstance(a, object))"),
            "each arm lowered by its own kind: {out}"
        );
    }

    #[test]
    fn union_negation_wraps_the_disjunction() {
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is not int | str
        "});
        assert!(
            out.contains("return not (isinstance(a, int) or isinstance(a, str))"),
            "`is not` negates the whole disjunction: {out}"
        );
    }

    #[test]
    fn union_three_arms() {
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is int | str | bytes
        "});
        assert!(
            out.contains(
                "return (isinstance(a, int) or isinstance(a, str) or isinstance(a, bytes))"
            ),
            "a flat chain of arms: {out}"
        );
    }

    #[test]
    fn union_none_arm_is_an_identity_check() {
        // `X | None` (an optional) tests the `None` arm by identity — `None` is
        // a value, so `isinstance(a, None)` would be a runtime `TypeError`
        let out = out(indoc! {"
            def f(a: object) -> bool:
                return a is int | None
        "});
        assert!(
            out.contains("return (isinstance(a, int) or a is None)"),
            "None arm is an identity check: {out}"
        );
    }

    #[test]
    fn union_effectful_lhs_binds_once_via_lambda() {
        // an effectful lhs must be evaluated exactly once across the arms, so it
        // is bound to a lambda parameter rather than referenced per arm
        let out = out(indoc! {"
            def g() -> object:
                return 1
            def f() -> bool:
                return g() is int | str
        "});
        assert!(
            out.contains(
                "return (lambda _by_is_value: isinstance(_by_is_value, int) or \
                 isinstance(_by_is_value, str))(g())"
            ),
            "effectful lhs bound once: {out}"
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

    #[test]
    fn stdlib_enum_member_rhs_keeps_identity() {
        // an enum member is a singleton instance, not a class, so
        // `isinstance(x, Color.RED)` would be a runtime TypeError; the pair
        // must keep `is` / `is not`
        let out = out(indoc! {"
            from enum import Enum

            class Color(Enum):
                RED = 1
                GREEN = 2

            print(Color.RED is Color.RED)
            print(Color.RED is not Color.GREEN)
        "});
        assert!(
            out.contains("print(Color.RED is Color.RED)"),
            "enum member rhs keeps identity: {out}"
        );
        assert!(
            out.contains("print(Color.RED is not Color.GREEN)"),
            "enum member rhs keeps identity under is not: {out}"
        );
    }

    #[test]
    fn caseless_based_variant_rhs_keeps_identity() {
        // the repro: a payload-less based-enum variant is a singleton instance
        let out = out(indoc! {"
            enum class Genre:
                case A, B

            print(Genre.A is not Genre.B)
        "});
        assert!(
            out.contains("print(Genre.A is not Genre.B)"),
            "caseless variant rhs keeps identity: {out}"
        );
    }

    #[test]
    fn payload_variant_class_rhs_lowers_but_unit_variant_kept() {
        // a payload variant resolves to a *class* (→ isinstance); a unit
        // variant in the same enum is a singleton instance (→ kept)
        let out = out(indoc! {"
            enum class Shape:
                case Circle(radius: float)
                case Point

            c = Shape.Circle(1.0)
            print(c is Shape.Circle)
            print(c is not Shape.Point)
        "});
        assert!(
            out.contains("print(isinstance(c, Shape.Circle))"),
            "payload variant rhs is a class and lowers to isinstance: {out}"
        );
        assert!(
            out.contains("print(c is not Shape.Point)"),
            "unit variant rhs is a singleton instance and keeps identity: {out}"
        );
    }
}
