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
//! How deeply a *checked* form validates is decided by what the target can
//! prove at runtime ([`TypeInfo::cast_check_plan`]):
//!
//! - a **user generic** whose instances carry `__orig_class__` (stamped by
//!   `A[int](…)`) validates its type arguments too, via `_parametric_is`:
//!   `x cast A[int]` rejects an `A[str]`.
//! - **anything else** collapses to the shallow `isinstance` target
//!   (`list[object]` → `list`, `int | str` → `(int, str)`), because builtins
//!   erase their arguments and a bare `isinstance(v, list[object])` is itself
//!   a runtime error. The dropped argument claim is reported by ty's
//!   `erased-cast-argument` lint.
//!
//! The unchecked `typing.cast` keeps the exact written type (it never reaches
//! `isinstance`).

use std::collections::BTreeSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::parametric_is::{PARAMETRIC_IS_RUNTIME, variance_tuple};
use crate::type_info::{SoundnessCheck, TypeInfo};

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

// deep forms, for a user generic whose instances carry `__orig_class__`
// (stamped by `A[int](…)`): validate the base class always, and the reified
// type arguments when the value carries them. a value with no reification
// passes the argument check — its arguments aren't available to check, leaving
// the base `isinstance` as the guarantee. reuses `_parametric_is` from
// `PARAMETRIC_IS_RUNTIME`
const CHECKED_CAST_P_HELPER: &str = "\
def _checked_cast_p(_v, _alias, _variances):
    _origin = getattr(_alias, \"__origin__\", _alias)
    if not isinstance(_v, _origin):
        raise TypeError(
            f\"cast to {_alias} failed: value is {type(_v).__name__}\"
        )
    if getattr(_v, \"__orig_class__\", None) is not None and not _parametric_is(_v, _alias, _variances):
        raise TypeError(
            f\"cast to {_alias} failed: value is {_v.__orig_class__}\"
        )
    return _v
";

const TRY_CAST_P_HELPER: &str = "\
def _try_cast_p(_v, _alias, _variances):
    _origin = getattr(_alias, \"__origin__\", _alias)
    if not isinstance(_v, _origin):
        return None
    if getattr(_v, \"__orig_class__\", None) is not None and not _parametric_is(_v, _alias, _variances):
        return None
    return _v
";

/// the runtime helper a `cast` / `cast?` occurrence lowers to
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Helper {
    /// `_checked_cast(value, target)` — shallow, raises on mismatch
    Checked,
    /// `_try_cast(value, target)` — shallow, yields `None` on mismatch
    Try,
    /// `_checked_cast_p(value, alias, variances)` — deep, raises on mismatch
    CheckedParametric,
    /// `_try_cast_p(value, alias, variances)` — deep, yields `None`
    TryParametric,
    /// `cast(type, value)` — unchecked `typing.cast`
    TypingCast,
}

impl Helper {
    /// the shallow or deep variant of this raising/yielding pair
    fn with_depth(self, parametric: bool) -> Self {
        match (self, parametric) {
            (Self::Checked | Self::CheckedParametric, true) => Self::CheckedParametric,
            (Self::Checked | Self::CheckedParametric, false) => Self::Checked,
            (Self::Try | Self::TryParametric, true) => Self::TryParametric,
            (Self::Try | Self::TryParametric, false) => Self::Try,
            (Self::TypingCast, _) => Self::TypingCast,
        }
    }

    fn open_call(self) -> &'static str {
        match self {
            Self::Checked => "_checked_cast(",
            Self::Try => "_try_cast(",
            Self::CheckedParametric => "_checked_cast_p(",
            Self::TryParametric => "_try_cast_p(",
            Self::TypingCast => "cast(",
        }
    }

    /// the preamble this helper's call needs
    fn runtime(self) -> &'static str {
        match self {
            Self::Checked => CHECKED_CAST_HELPER,
            Self::Try => TRY_CAST_HELPER,
            Self::CheckedParametric => CHECKED_CAST_P_HELPER,
            Self::TryParametric => TRY_CAST_P_HELPER,
            Self::TypingCast => "from typing import cast",
        }
    }

    /// deep helpers call `_parametric_is`, so they pull in its runtime too
    fn is_parametric(self) -> bool {
        matches!(self, Self::CheckedParametric | Self::TryParametric)
    }
}

struct CastLower<'a> {
    types: &'a dyn TypeInfo,
    /// `true` when the checked (`cast`) form does its runtime check; `false`
    /// degrades `cast` to unchecked `typing.cast`
    checked: bool,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    used: BTreeSet<Helper>,
}

impl<'a> CastLower<'a> {
    fn new(types: &'a dyn TypeInfo, checked: bool) -> Self {
        Self {
            types,
            checked,
            edits: Vec::new(),
            used: BTreeSet::new(),
        }
    }

    /// the target arguments for a *runtime-checked* cast, and the helper
    /// variant they belong to. a user generic whose instances carry
    /// `__orig_class__` goes deep (`A[int]` plus the variance codes
    /// `_parametric_is` needs); anything else collapses to the shallow
    /// `isinstance` target ty derives (`list[object]` → `list`,
    /// `int | str` → `(int, str)`), since a bare `isinstance(v, list[object])`
    /// is a runtime error. with no faithful test the written type passes
    /// through unchanged
    fn target(&self, type_arg: &Expr, helper: Helper) -> (Helper, Vec<Fragment>) {
        match self.types.cast_check_plan(type_arg) {
            Some(SoundnessCheck::Parametric { alias, variances }) => (
                helper.with_depth(true),
                vec![Fragment::Lit(format!(
                    "{alias}, {}",
                    variance_tuple(&variances)
                ))],
            ),
            Some(SoundnessCheck::Isinstance(target)) => {
                (helper.with_depth(false), vec![Fragment::Lit(target)])
            }
            None => (
                helper.with_depth(false),
                vec![Fragment::Src(type_arg.range())],
            ),
        }
    }

    fn emit(&mut self, whole: TextRange, type_arg: &Expr, value_range: TextRange, helper: Helper) {
        // unchecked `typing.cast(type, value)` keeps the exact written type (it
        // never reaches `isinstance`) and takes the type first
        let (helper, first, rest) = if helper == Helper::TypingCast {
            (
                helper,
                Fragment::Src(type_arg.range()),
                vec![Fragment::Src(value_range)],
            )
        } else {
            let (helper, rest) = self.target(type_arg, helper);
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
            let helper = if call.is_checked_cast {
                Helper::Try
            } else if self.checked {
                Helper::Checked
            } else {
                Helper::TypingCast
            };
            self.emit(expr.range(), type_arg, value_arg.range(), helper);
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
        // `_parametric_is` must precede the deep helpers that call it
        if inner.used.iter().any(|helper| helper.is_parametric()) {
            ctx.required_imports.push(PARAMETRIC_IS_RUNTIME.to_owned());
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
            out.contains("b = _checked_cast_p(x, A[int], (0,))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _checked_cast_p"), "got:\n{out}");
        // the deep helper calls `_parametric_is`, so its runtime comes along
        assert!(out.contains("def _parametric_is"), "got:\n{out}");
    }

    #[test]
    fn user_generic_try_target_checks_arguments() {
        let out = check(
            "class A[T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast? A[int]\n",
        );
        assert!(
            out.contains("b = _try_cast_p(x, A[int], (0,))"),
            "got:\n{out}"
        );
        assert!(out.contains("def _try_cast_p"), "got:\n{out}");
    }

    /// the variance codes come from the target's own type parameters, so an
    /// `out T` matches covariantly at runtime
    #[test]
    fn user_generic_target_carries_variance() {
        let out = check(
            "class A[out T]:\n    t: T\n    def __init__(self, t: T): ...\n\ndef f(x: object):\n    b = x cast A[int]\n",
        );
        assert!(
            out.contains("b = _checked_cast_p(x, A[int], (1,))"),
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
    fn parameterized_generic_collapses_to_origin() {
        // regression: `isinstance(v, list[object])` is a runtime error; the
        // checked cast must target the origin `list` instead
        let out = check("def f(a: object):\n    b = a cast list[object]\n");
        assert!(out.contains("b = _checked_cast(a, list)"), "got:\n{out}");
        assert!(
            !out.contains("list[object]"),
            "no parameterized target:\n{out}"
        );
    }

    #[test]
    fn union_of_generics_collapses_each_arm() {
        let out = check("def f(a: object):\n    b = a cast? list[int] | None\n");
        assert!(
            out.contains("b = _try_cast(a, (list, type(None)))"),
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
        assert!(out.contains("_checked_cast([1], list)"), "got:\n{out}");
        assert_eq!(
            out.matches('(').count(),
            out.matches(')').count(),
            "balanced parens:\n{out}"
        );
    }

    #[test]
    fn cast_identifier_unaffected() {
        // plain `cast(...)` and `cast = ...` stay untouched
        let out = check("cast = 5\nb = cast\n");
        assert!(!out.contains("_checked_cast"), "got:\n{out}");
    }
}
