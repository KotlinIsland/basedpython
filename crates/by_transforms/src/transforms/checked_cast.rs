//! Lowering for the `cast` and `cast?` infix operators.
//!
//! Both parse as an `ExprCall` whose `func` is a synthetic `Name("cast")` and
//! whose `arguments` are `[type, value]`; a flag distinguishes them:
//!
//! - **`is_cast`** — `<value> cast <type>`, the *checked* cast. By default
//!   (`Config::checked_cast`) it verifies the value at runtime and raises a
//!   `TypeError` on a mismatch, so `"1" cast int` errors. Its type is `type`.
//!   When checked casts are disabled it degrades to the unchecked
//!   `typing.cast(type, value)`.
//! - **`is_checked_cast`** — `<value> cast? <type>`, the *safe* cast. Always
//!   available: it yields the value when `isinstance(value, type)` holds and
//!   `None` otherwise, so its type is `type | None`.
//!
//! Each is rewritten to a helper call (or plain `cast` when unchecked). The
//! value is a [`Fragment::Src`] passthrough, so lowerings inside it still
//! compose and it is evaluated exactly once.
//!
//! How a *checked* form validates is decided by the **same engine that decides
//! `x is T`** — [`build_predicate`], via [`TypeInfo::parametric_cast_plan`]. The
//! two forms ask one question (does this value satisfy this specialization at
//! runtime) and differ only in two parameters:
//!
//! - [`TargetPosition::Type`], because a cast's target is a *type* expression
//!   while an `is`-rhs is a value expression, so ty infers it differently;
//! - [`ProbeStrictness::Lenient`], because a cast is an assertion: arguments the
//!   runtime cannot see are not held against the value, keeping
//!   `[1, 2] cast list[int]` legal. An `is`-test is strict — a `True` narrows,
//!   so it must be earned.
//!
//! Everything else follows from the shared plan: a reified type parameter
//! compares its runtime cell (`def f[T](x: list[T])` casting to `list[int]`
//! lowers to `T == int`), a user generic probes `__orig_class__`, a protocol is
//! checked structurally, and a union is the disjunction of its arms, each
//! lowered by its own kind. Such a target lowers to `_checked_cast_pred(value,
//! lambda …)` so the value is evaluated once and the predicate can reference it.
//!
//! A target with **no parametric claim** keeps the compact shallow form
//! (`_checked_cast(v, (int, str))`), and one the engine cannot check at all
//! degrades to `typing.cast` and is reported by ty's `erased-cast-argument`.
//!
//! The unchecked `typing.cast` keeps the exact written type (it never reaches
//! `isinstance`).

use std::collections::BTreeSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::parametric_is::{
    PARAMETRIC_IS_RUNTIME, PROTOCOL_IS_RUNTIME, ProbeStrictness, TargetPosition, build_predicate,
};
use crate::type_info::{CastCheck, SoundnessCheck, TypeInfo};

/// the lambda parameter a predicate-form cast binds its value to, so the value
/// is evaluated exactly once and the predicate can reference it
const CAST_VALUE_PARAM: &str = "_by_cast_value";

// `<value> cast <type>` with checks on: verify at runtime, raise on mismatch.
const CHECKED_CAST_HELPER: &str = "\
def _checked_cast(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f\"cast to {getattr(_t, '__name__', _t)} failed: value is {type(_v).__name__}\"
        )
    return _v
";

// `<value> cast? <type>`: yield the value when it matches, else `None`.
const TRY_CAST_HELPER: &str = "\
def _try_cast(_v, _t):
    return _v if isinstance(_v, _t) else None
";

// predicate forms, for any target the shared parametric engine can decide at
// runtime — a reified-cell comparison (`T == int`), an `__orig_class__` probe, a
// structural protocol check, or a disjunction of those across a union's arms.
// the predicate is a lambda so the value is evaluated exactly once (as `_v`) and
// referenced from inside the test
const CHECKED_CAST_PRED_HELPER: &str = "\
def _checked_cast_pred(_v, _pred):
    if not _pred(_v):
        raise TypeError(f\"cast failed: value is {type(_v).__name__}\")
    return _v
";

const TRY_CAST_PRED_HELPER: &str = "\
def _try_cast_pred(_v, _pred):
    return _v if _pred(_v) else None
";

/// the runtime helper a `cast` / `cast?` occurrence lowers to
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Helper {
    /// `_checked_cast(value, target)` — shallow `isinstance`, raises on mismatch
    Checked,
    /// `_try_cast(value, target)` — shallow `isinstance`, yields `None`
    Try,
    /// `_checked_cast_pred(value, pred)` — shared parametric predicate, raises
    CheckedPredicate,
    /// `_try_cast_pred(value, pred)` — shared parametric predicate, yields `None`
    TryPredicate,
    /// `cast(type, value)` — unchecked `typing.cast`
    TypingCast,
}

impl Helper {
    /// whether this helper yields `None` on mismatch (the `cast?` family) rather
    /// than raising (the `cast` family)
    fn is_yielding(self) -> bool {
        matches!(self, Self::Try | Self::TryPredicate)
    }

    /// the predicate-taking variant of this raising/yielding pair
    fn as_predicate(self) -> Self {
        if self.is_yielding() {
            Self::TryPredicate
        } else {
            Self::CheckedPredicate
        }
    }

    fn open_call(self) -> &'static str {
        match self {
            Self::Checked => "_checked_cast(",
            Self::Try => "_try_cast(",
            Self::CheckedPredicate => "_checked_cast_pred(",
            Self::TryPredicate => "_try_cast_pred(",
            Self::TypingCast => "cast(",
        }
    }

    /// the preamble this helper's call needs
    fn runtime(self) -> &'static str {
        match self {
            Self::Checked => CHECKED_CAST_HELPER,
            Self::Try => TRY_CAST_HELPER,
            Self::CheckedPredicate => CHECKED_CAST_PRED_HELPER,
            Self::TryPredicate => TRY_CAST_PRED_HELPER,
            Self::TypingCast => "from typing import cast",
        }
    }
}

struct CastLower<'a> {
    types: &'a dyn TypeInfo,
    /// `true` when the checked (`cast`) form does its runtime check; `false`
    /// degrades `cast` to unchecked `typing.cast`
    checked: bool,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    used: BTreeSet<Helper>,
    needs_parametric: bool,
    needs_protocol: bool,
}

impl<'a> CastLower<'a> {
    fn new(types: &'a dyn TypeInfo, checked: bool) -> Self {
        Self {
            types,
            checked,
            edits: Vec::new(),
            used: BTreeSet::new(),
            needs_parametric: false,
            needs_protocol: false,
        }
    }

    /// the call arguments after the value, and the helper they belong to.
    ///
    /// a target the shared parametric engine can decide — a reified-cell
    /// comparison, an `__orig_class__` probe, a structural protocol check, or a
    /// union mixing those — becomes a predicate lambda built by exactly the code
    /// that builds an `is`-test. anything with no parametric claim keeps the
    /// compact shallow `isinstance` target ty derives (`list[object]` → `list`,
    /// `int | str` → `(int, str)`), since a bare `isinstance(v, list[object])`
    /// is itself a runtime error
    fn target(
        &mut self,
        value_arg: &Expr,
        type_arg: &Expr,
        helper: Helper,
    ) -> (Helper, Vec<Fragment>) {
        let value_ref = || Fragment::Lit(CAST_VALUE_PARAM.to_owned());
        let (predicate, needs) = build_predicate(
            self.types,
            &value_ref,
            value_arg,
            type_arg,
            TargetPosition::Type,
            ProbeStrictness::Lenient,
        );
        if !needs.all_plain && !needs.erased {
            self.needs_parametric |= needs.parametric_runtime;
            self.needs_protocol |= needs.protocol_runtime;
            let mut fragments = vec![Fragment::Lit(format!("lambda {CAST_VALUE_PARAM}: "))];
            fragments.extend(predicate);
            return (helper.as_predicate(), fragments);
        }
        // no parametric claim (or none that can be checked): the shallow
        // `isinstance` target, or the written type when ty offers no faithful one
        match self.types.cast_check_plan(type_arg) {
            Some(CastCheck::Kind(SoundnessCheck::Isinstance(target))) => {
                (helper, vec![Fragment::Lit(target)])
            }
            _ => (helper, vec![Fragment::Src(type_arg.range())]),
        }
    }

    fn emit(&mut self, whole: TextRange, type_arg: &Expr, value_arg: &Expr, helper: Helper) {
        let value_range = value_arg.range();
        // unchecked `typing.cast(type, value)` keeps the exact written type (it
        // never reaches `isinstance`) and takes the type first
        let (helper, first, rest) = if helper == Helper::TypingCast {
            (
                helper,
                Fragment::Src(type_arg.range()),
                vec![Fragment::Src(value_range)],
            )
        } else {
            let (helper, rest) = self.target(value_arg, type_arg, helper);
            (helper, Fragment::Src(value_range), rest)
        };

        self.used.insert(helper);
        let mut fragments = vec![
            Fragment::Lit(helper.open_call().to_owned()),
            first,
            Fragment::Lit(", ".to_owned()),
        ];
        fragments.extend(rest);
        fragments.push(Fragment::Lit(")".to_owned()));
        self.edits.push((whole, fragments));
    }
}

impl<'ast> Visitor<'ast> for CastLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && (call.is_cast || call.is_checked_cast)
            && let [type_arg, value_arg] = &*call.arguments.args
        {
            // a statically-proven upcast needs no runtime check — the probe
            // would always pass, and for a subscripted protocol or builtin
            // target it cannot even run — so it degrades to a plain
            // `typing.cast`, exactly as a disabled checked cast does. a
            // method-bearing protocol target has no faithful runtime check at
            // all, so it degrades the same way rather than emit an `isinstance`
            // against the protocol (a runtime error)
            let redundant = self.types.cast_is_redundant(value_arg, type_arg)
                || self.types.cast_target_is_unverifiable(type_arg);
            let helper = if call.is_checked_cast && !redundant {
                Helper::Try
            } else if self.checked && !redundant {
                Helper::Checked
            } else {
                Helper::TypingCast
            };
            self.emit(expr.range(), type_arg, value_arg, helper);
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct CheckedCastPass {
    checked: bool,
}

impl CheckedCastPass {
    pub(crate) fn new(checked: bool) -> Self {
        Self { checked }
    }
}

impl TypeAwarePass for CheckedCastPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = CastLower::new(types, self.checked);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if inner.edits.is_empty() {
            return;
        }
        // `_parametric_is` / `_by_protocol_is` must precede the predicates that
        // call them
        if inner.needs_parametric {
            ctx.required_imports.push(PARAMETRIC_IS_RUNTIME.to_owned());
        }
        if inner.needs_protocol {
            ctx.required_imports.push(PROTOCOL_IS_RUNTIME.to_owned());
        }
        for helper in &inner.used {
            ctx.required_imports.push(helper.runtime().to_owned());
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use ruff_python_ast::PythonVersion;

    /// default config — checked casts on
    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    fn unchecked_config() -> Config {
        Config {
            checked_cast: false,
            ..Config::test_default()
        }
    }

    #[test]
    fn cast_is_checked_by_default() {
        let out = check("def f(a: object):\n    b = a cast int\n");
        assert!(out.contains("b = _checked_cast(a, int)"), "got:\n{out}");
        assert!(
            out.contains("raise TypeError"),
            "strict helper injected:\n{out}"
        );
    }

    #[test]
    fn cast_disabled_is_typing_cast() {
        let out = transpile(
            "def f(a: object):\n    b = a cast int\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(int, a)"), "got:\n{out}");
        assert!(
            out.contains("from typing import cast"),
            "import injected:\n{out}"
        );
        assert!(!out.contains("_checked_cast"), "got:\n{out}");
    }

    #[test]
    fn try_cast_returns_none_and_is_ungated() {
        // `cast?` is always the safe form, independent of the checked-cast config
        for config in [Config::test_default(), unchecked_config()] {
            let out = transpile("def f(a: object):\n    b = a cast? int\n", &config).unwrap();
            assert!(out.contains("b = _try_cast(a, int)"), "got:\n{out}");
            assert!(out.contains("def _try_cast"), "got:\n{out}");
        }
    }

    #[test]
    fn checked_cast_to_union() {
        let out = check("def f(a: object):\n    b = a cast int | str\n");
        assert!(
            out.contains("b = _checked_cast(a, (int, str))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn try_cast_to_union() {
        let out = check("def f(a: object):\n    b = a cast? int | str\n");
        assert!(out.contains("b = _try_cast(a, (int, str))"), "got:\n{out}");
    }

    /// a user generic's instances carry `__orig_class__`, so the cast can
    /// validate the type arguments rather than just the base class
    #[test]
    fn user_generic_target_checks_arguments() {
        let out = check(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_pred(x, lambda _by_cast_value: _parametric_is_lenient(_by_cast_value, A[int], (0,)))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _checked_cast_pred"), "got:\n{out}");
        // the deep helper calls `_parametric_is`, so its runtime comes along
        assert!(out.contains("def _parametric_is"), "got:\n{out}");
    }

    #[test]
    fn user_generic_try_target_checks_arguments() {
        let out = check(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast? A[int]\n",
        );
        assert!(
            out.contains("b = _try_cast_pred(x, lambda _by_cast_value: _parametric_is_lenient(_by_cast_value, A[int], (0,)))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _try_cast_pred"), "got:\n{out}");
    }

    /// the variance codes come from the target's own type parameters, so an
    /// `out T` matches covariantly at runtime
    #[test]
    fn user_generic_target_carries_variance() {
        let out = check(
            "class A[out T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_pred(x, lambda _by_cast_value: _parametric_is_lenient(_by_cast_value, A[int], (1,)))"),
            "got:\n{out}"
        );
    }

    /// an unchecked cast never reaches a runtime probe, so a user generic
    /// target stays a plain `typing.cast`
    #[test]
    fn user_generic_unchecked_is_typing_cast() {
        let out = transpile(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(A[int], x)"), "got:\n{out}");
        assert!(!out.contains("_parametric_is"), "got:\n{out}");
    }

    #[test]
    fn parameterized_builtin_probes_leniently() {
        // regression: `isinstance(v, list[object])` is a runtime error, so the
        // subscripted target must never reach `isinstance`. the probe unwraps it
        // to the origin itself, and checks the arguments only when the value
        // records them
        let out = check("def f(a: object):\n    b = a cast list[object]\n");
        assert!(
            out.contains("b = _checked_cast_pred(a, lambda _by_cast_value: _parametric_is_lenient(_by_cast_value, list[object], (0,)))"),
            "got:\n{out}"
        );
    }

    /// a union target is the disjunction of its arms, each lowered by its own
    /// kind — the same decomposition an `is`-test uses. it must never become a
    /// single `isinstance` against a tuple containing a parameterized arm
    #[test]
    fn union_arms_are_decomposed() {
        let out = check("def f(a: object):\n    b = a cast? list[int] | None\n");
        assert!(
            out.contains("b = _try_cast_pred(a, lambda _by_cast_value: _parametric_is_lenient(_by_cast_value, list[int], (0,)) or _by_cast_value is None)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn unchecked_keeps_written_type() {
        // `typing.cast` never reaches `isinstance`, so the exact type is kept
        let out = transpile(
            "def f(a: object):\n    b = a cast list[object]\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(list[object], a)"), "got:\n{out}");
    }

    #[test]
    fn value_evaluated_once() {
        let out = check("def f():\n    b = g() cast int\n");
        assert_eq!(out.matches("g()").count(), 1, "single evaluation:\n{out}");
    }

    #[test]
    fn both_forms_in_one_file() {
        let out = check("def f(a: object, b: object):\n    x = a cast int\n    y = b cast? str\n");
        assert!(out.contains("x = _checked_cast(a, int)"), "got:\n{out}");
        assert!(out.contains("y = _try_cast(b, str)"), "got:\n{out}");
    }

    #[test]
    fn collection_literal_value_composes() {
        // a bare collection-literal value passes through the wrap intact
        let out = check("x = [1] cast list[object]\n");
        assert!(out.contains("_checked_cast_pred([1], "), "got:\n{out}");
        assert_eq!(
            out.matches('(').count(),
            out.matches(')').count(),
            "balanced parens:\n{out}"
        );
    }

    /// a statically-proven upcast needs no runtime check: casting a value
    /// already known to be the target degrades to a plain `typing.cast`, which
    /// avoids a probe that (for a subscripted builtin) drops the argument claim
    /// and (for a subscripted protocol) would be a runtime error
    #[test]
    fn redundant_upcast_is_typing_cast() {
        let out = check("class B[T](list[T]): ...\n\ndef f():\n    b = B[int]() cast list[int]\n");
        assert!(out.contains("b = cast(list[int], B[int]())"), "got:\n{out}");
        assert!(!out.contains("_checked_cast"), "no runtime probe:\n{out}");
    }

    /// the same upcast through `cast?` also degrades — the value always matches,
    /// so no probe or `None` arm is needed at runtime
    #[test]
    fn redundant_try_upcast_is_typing_cast() {
        let out = check("class B[T](list[T]): ...\n\ndef f():\n    b = B[int]() cast? list[int]\n");
        assert!(out.contains("b = cast(list[int], B[int]())"), "got:\n{out}");
        assert!(!out.contains("_try_cast"), "no runtime probe:\n{out}");
    }

    /// subclassing a subscripted protocol is a redundant upcast too — a bare
    /// `isinstance(v, Sequence[object])` would raise, so it must not be emitted
    #[test]
    fn redundant_upcast_to_protocol_is_typing_cast() {
        let out = check(
            "from collections.abc import Sequence\n\nclass A[T](Sequence[T]):\n    def __getitem__(self, i): ...  # type: ignore\n    def __len__(self): ...\n\ndef f():\n    a = A[int]() cast Sequence[object]\n",
        );
        assert!(
            out.contains("a = cast(Sequence[object], A[int]())"),
            "got:\n{out}"
        );
        assert!(
            !out.contains("_checked_cast") && !out.contains("_parametric_is"),
            "no runtime probe:\n{out}"
        );
    }

    /// a genuine cast whose value is *not* already the target keeps its probe
    #[test]
    fn non_redundant_cast_keeps_probe() {
        // (the probe is lenient: an unreified value passes the argument check)
        let out = check("def f(a: object):\n    b = a cast list[int]\n");
        assert!(
            out.contains("b = _checked_cast_pred(a, lambda _by_cast_value: _parametric_is_lenient(_by_cast_value, list[int], (0,)))"),
            "got:\n{out}"
        );
    }

    /// a data-member protocol target has no `__orig_class__` to probe, but its
    /// members are checked structurally against the value's reified annotations
    #[test]
    fn data_member_protocol_cast_checks_structurally() {
        let out = check(
            "from typing import Protocol\n\nclass A[T](Protocol):\n    a: T\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_pred(x, lambda _by_cast_value: _by_protocol_is(_by_cast_value, [(\"attr\", \"a\", int, 0)]))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _checked_cast_pred"), "got:\n{out}");
        // the structural helper calls `_by_protocol_is`, so its runtime comes along
        assert!(out.contains("def _by_protocol_is"), "got:\n{out}");
    }

    #[test]
    fn data_member_protocol_try_cast_checks_structurally() {
        let out = check(
            "from typing import Protocol\n\nclass A[T](Protocol):\n    a: T\n\ndef f(x: object):\n    b = x cast? A[bool]\n",
        );
        assert!(
            out.contains("b = _try_cast_pred(x, lambda _by_cast_value: _by_protocol_is(_by_cast_value, [(\"attr\", \"a\", bool, 0)]))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _try_cast_pred"), "got:\n{out}");
    }

    /// a method member is checkable too — its return type is validated against
    /// the value method's reified return annotation
    #[test]
    fn method_protocol_cast_checks_structurally() {
        let out = check(
            "from typing import Protocol\n\nclass M[T](Protocol):\n    def get(self) -> T: ...\n\ndef f(x: object):\n    b = x cast M[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_pred(x, lambda _by_cast_value: _by_protocol_is(_by_cast_value, [(\"method\", \"get\", [], (int, 1))]))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _by_method_matches"), "got:\n{out}");
    }

    /// a protocol whose member has no runtime spelling (a callable attribute)
    /// still has no runtime residue, so the checked cast degrades to
    /// `typing.cast` rather than an `isinstance` against the protocol
    #[test]
    fn unspellable_protocol_cast_degrades_to_typing_cast() {
        let out = check(
            "from typing import Protocol\nfrom collections.abc import Callable\n\nclass M[T](Protocol):\n    cb: Callable[[T], T]\n\ndef f(x: object):\n    b = x cast M[int]\n",
        );
        assert!(out.contains("b = cast(M[int], x)"), "got:\n{out}");
        assert!(
            !out.contains("_checked_cast") && !out.contains("_by_protocol_is"),
            "no runtime probe against an unspellable protocol:\n{out}"
        );
    }

    /// the same degradation applies to `cast?`
    #[test]
    fn unspellable_protocol_try_cast_degrades_to_typing_cast() {
        let out = check(
            "from typing import Protocol\nfrom collections.abc import Callable\n\nclass M[T](Protocol):\n    cb: Callable[[T], T]\n\ndef f(x: object):\n    b = x cast? M[int]\n",
        );
        assert!(out.contains("b = cast(M[int], x)"), "got:\n{out}");
        assert!(!out.contains("_try_cast"), "no probe:\n{out}");
    }

    /// an unchecked config keeps a data-member protocol cast as a plain
    /// `typing.cast` — no structural probe is emitted
    #[test]
    fn data_member_protocol_unchecked_is_typing_cast() {
        let out = transpile(
            "from typing import Protocol\n\nclass A[T](Protocol):\n    a: T\n\ndef f(x: object):\n    b = x cast A[int]\n",
            &unchecked_config(),
        )
        .unwrap();
        assert!(out.contains("b = cast(A[int], x)"), "got:\n{out}");
        assert!(!out.contains("_by_protocol_is"), "got:\n{out}");
    }

    /// a value typed by a *reified* type parameter carries the answer in a
    /// runtime cell, so the cast is checked exactly — no argument is assumed.
    /// this is the same `TokenEq` lowering `x is list[int]` uses
    #[test]
    fn reified_type_parameter_compares_its_cell() {
        let out = transpile(
            "def f[T](data: list[T]):\n    x = data cast? list[int]\n    return x\n",
            &Config {
                min_version: PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("x = _try_cast_pred(data, lambda _by_cast_value: (T == int))"),
            "reified cell compared: {out}"
        );
        assert!(
            out.contains("@generic  # basedpython: reified"),
            "the parametric cast must reify T: {out}"
        );
    }

    /// a union mixing a user generic with a plain class lowers each arm by its
    /// own kind, exactly as the `is`-test does
    #[test]
    fn union_mixes_probe_and_isinstance_per_arm() {
        let out = check(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(a: object):\n    b = a cast A[int] | str\n",
        );
        assert!(
            out.contains(
                "b = _checked_cast_pred(a, lambda _by_cast_value: \
                 _parametric_is_lenient(_by_cast_value, A[int], (0,)) \
                 or isinstance(_by_cast_value, str))"
            ),
            "each arm lowered by its own kind: {out}"
        );
    }

    #[test]
    fn cast_identifier_unaffected() {
        // plain `cast(...)` and `cast = ...` stay untouched
        let out = check("cast = 5\nb = cast\n");
        assert!(!out.contains("_checked_cast"), "got:\n{out}");
    }
}
